//! ublk target: exposes a wear-leveled volume as `/dev/ublkbN`.
//!
//! Device creation, parameters and lifecycle follow libublk's
//! `examples/loop.rs`; the per-request logic lives in [`io`].

mod io;

use crate::mapper::ChunkMapper;
use crate::ondisk::Layout;
use crate::persist::FlushSignal;
use anyhow::{anyhow, Context, Result};
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::UblkDev;
use libublk::{UblkError, UblkFlags};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Fixed-file index of the backing device inside each queue's io_uring (0 is the ublk char dev).
pub const BACKING_FIXED_FD: u32 = 1;

/// Name reported to libublk and stored in the exported JSON.
pub const TARGET_TYPE: &str = "wlvl";

#[derive(Debug, Clone)]
pub struct TargetOptions {
    /// ublk device id, -1 for auto allocation.
    pub dev_id: i32,
    pub nr_queues: u16,
    pub depth: u16,
    pub io_buf_bytes: u32,
}

impl Default for TargetOptions {
    fn default() -> Self {
        TargetOptions {
            dev_id: -1,
            nr_queues: 1,
            depth: 64,
            io_buf_bytes: 512 * 1024,
        }
    }
}

/// State shared by every queue thread.
pub struct Target {
    pub uuid: Uuid,
    pub layout: Layout,
    pub mapper: Arc<ChunkMapper>,
    /// Woken on every FLUSH so the persister can commit counters.
    pub flush_signal: Arc<FlushSignal>,
    pub backing_path: PathBuf,
    /// Read back one in every N written segments (0 = never).
    pub verify_every: u32,
    backing: File,
    sector_shifts: (u8, u8),
}

impl Target {
    pub fn new(
        uuid: Uuid,
        layout: Layout,
        mapper: Arc<ChunkMapper>,
        flush_signal: Arc<FlushSignal>,
        backing_path: &Path,
        backing: File,
        sector_shifts: (u8, u8),
    ) -> Target {
        Target {
            uuid,
            layout,
            mapper,
            flush_signal,
            backing_path: backing_path.to_path_buf(),
            verify_every: 0,
            backing,
            sector_shifts,
        }
    }

    pub fn with_verify_every(mut self, n: u32) -> Target {
        self.verify_every = n;
        self
    }

    fn init_dev(&self, dev: &mut UblkDev) -> std::result::Result<(), UblkError> {
        let fd = self.backing.as_raw_fd();
        // SAFETY: plain fcntl on an fd we own.
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, libc::O_DIRECT);
        }
        let tgt = &mut dev.tgt;
        let nr_fds = tgt.nr_fds;
        tgt.fds[nr_fds as usize] = fd;
        tgt.nr_fds = nr_fds + 1;
        debug_assert_eq!(nr_fds as u32, BACKING_FIXED_FD);

        let (lbs, pbs) = self.sector_shifts;
        tgt.dev_size = self.layout.exported_bytes();
        tgt.params = libublk::sys::ublk_params {
            types: libublk::sys::UBLK_PARAM_TYPE_BASIC,
            basic: libublk::sys::ublk_param_basic {
                // The card has a volatile write cache: ask the kernel to send FLUSH.
                attrs: libublk::sys::UBLK_ATTR_VOLATILE_CACHE,
                logical_bs_shift: lbs,
                physical_bs_shift: pbs,
                io_opt_shift: pbs.max(12),
                io_min_shift: lbs,
                max_sectors: dev.dev_info.max_io_buf_bytes >> 9,
                dev_sectors: tgt.dev_size >> 9,
                ..Default::default()
            },
            ..Default::default()
        };
        dev.set_target_json(serde_json::json!({
            TARGET_TYPE: {
                "uuid": self.uuid.to_string(),
                "backing": self.backing_path.display().to_string(),
                "chunk_size": self.layout.chunk_size,
            }
        }));
        Ok(())
    }
}

/// Create the ublk device and serve it until it is deleted.
///
/// `on_started` runs once `/dev/ublkbN` exists and receives the control handle;
/// `ctrl_slot` (if given) is filled with the control handle so another thread
/// (signal handler, control socket) can call `kill_dev`.
pub fn run<F>(
    target: Arc<Target>,
    opts: &TargetOptions,
    ctrl_slot: Option<Arc<Mutex<Option<Arc<UblkCtrl>>>>>,
    on_started: F,
) -> Result<()>
where
    F: FnOnce(&UblkCtrl) + Send + Sync + 'static,
{
    let ctrl = Arc::new(
        UblkCtrlBuilder::default()
            .name(TARGET_TYPE)
            .id(opts.dev_id)
            .nr_queues(opts.nr_queues)
            .depth(opts.depth)
            .io_buf_bytes(opts.io_buf_bytes)
            .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
            .build()
            .map_err(|e| anyhow!("creating ublk control device: {e}"))?,
    );
    if let Some(slot) = &ctrl_slot {
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctrl.clone());
    }

    let init_target = target.clone();
    let queue_target = target.clone();
    ctrl.run_target(
        move |dev: &mut UblkDev| init_target.init_dev(dev),
        move |qid, dev: &UblkDev| io::queue_main(qid, dev, &queue_target),
        on_started,
    )
    .map_err(|e| anyhow!("running ublk target: {e}"))?;

    if let Some(slot) = &ctrl_slot {
        slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
    // Device may already be gone if it was deleted from elsewhere.
    let _ = ctrl.del_dev();
    Ok(())
}

/// A live ublk device created by `wlvl`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AttachedDevice {
    pub dev_id: u32,
    pub bdev_path: String,
    pub uuid: Uuid,
    pub backing: String,
}

/// All running `wlvl` ublk devices (other targets' devices are skipped).
pub fn list_attached() -> Vec<AttachedDevice> {
    let ids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = ids.clone();
    UblkCtrl::for_each_dev_id(move |id| sink.lock().unwrap_or_else(|e| e.into_inner()).push(id));
    let mut ids = ids.lock().unwrap_or_else(|e| e.into_inner()).clone();
    ids.sort_unstable();

    let mut out = Vec::new();
    for id in ids {
        let Ok(ctrl) = UblkCtrl::new_simple(id as i32) else {
            continue;
        };
        let Some(data) = ctrl.get_target_data_from_json() else {
            continue;
        };
        let Some(wlvl) = data.get(TARGET_TYPE) else {
            continue;
        };
        let Some(uuid) = wlvl
            .get("uuid")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        out.push(AttachedDevice {
            dev_id: id,
            bdev_path: ctrl.get_bdev_path(),
            uuid,
            backing: wlvl
                .get("backing")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}

pub fn find_attached(uuid: Uuid) -> Option<AttachedDevice> {
    list_attached().into_iter().find(|d| d.uuid == uuid)
}

/// Delete a ublk device by id. The serving process notices and exits its queue loop.
pub fn delete_device(dev_id: u32) -> Result<()> {
    let ctrl = UblkCtrl::new_simple(dev_id as i32)
        .map_err(|e| anyhow!("opening ublk device {dev_id}: {e}"))?;
    ctrl.del_dev()
        .map_err(|e| anyhow!("deleting ublk device {dev_id}: {e}"))?;
    Ok(())
}

/// Put the daemon in its own mount namespace (see libublk README: a process
/// that shares a mount namespace with users of its own `/dev/ublkbN` can
/// deadlock on exit). Needs CAP_SYS_ADMIN; failure is reported, not fatal.
pub fn unshare_mount_namespace() -> Result<()> {
    nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNS).context("unshare(CLONE_NEWNS)")
}
