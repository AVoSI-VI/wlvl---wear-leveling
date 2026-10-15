//! Volume lifecycle shared by `wlvl attach` (one volume) and `wlvl daemon` (many).
//!
//! `serve_volume` opens a formatted device, starts the persister, the wear
//! worker and the ublk target, registers the volume on the control plane and
//! blocks until the ublk device is deleted (detach, signal, or error).

use crate::backing::{self, BackingDevice};
use crate::config::{Config, DeviceConfig};
use crate::control::{self, LiveVolume, Registry};
use crate::health::HealthParams;
use crate::mapper::ChunkMapper;
use crate::ondisk::Volume;
use crate::persist::{self, FlushSignal, PersistOptions, Persister};
use crate::ublk_target::{self, Target, TargetOptions};
use crate::wear::{self, PolicyParams, WearOptions};
use anyhow::{bail, Context, Result};
use libublk::ctrl::UblkCtrl;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// Shared slot through which a signal handler can reach a running device.
pub type CtrlSlot = Arc<Mutex<Option<Arc<UblkCtrl>>>>;

pub fn new_ctrl_slot() -> CtrlSlot {
    Arc::new(Mutex::new(None))
}

/// Ask a running device to stop; the serving thread then unwinds.
pub fn kill(slot: &CtrlSlot) {
    if let Some(ctrl) = slot.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        if let Err(e) = ctrl.kill_dev() {
            tracing::warn!(error = %e, "kill_dev failed");
        }
    }
}

/// Serve one volume until its ublk device goes away.
pub fn serve_volume<F>(
    cfg: &DeviceConfig,
    flush_interval: Duration,
    registry: Arc<Registry>,
    ctrl_slot: CtrlSlot,
    on_started: F,
) -> Result<()>
where
    F: FnOnce(&str) + Send + Sync + 'static,
{
    let path = backing::resolve(cfg.uuid, cfg.path.as_deref())
        .with_context(|| format!("resolving device for {}", cfg.label()))?;
    if backing::is_mounted(&path).unwrap_or(false) {
        bail!(
            "{} (or a partition of it) is mounted; unmount first",
            path.display()
        );
    }
    let dev =
        BackingDevice::open(&path, true).with_context(|| format!("opening {}", path.display()))?;
    let sector_shifts = dev.sector_shifts().context("querying sector sizes")?;
    let vol =
        Volume::open(dev).with_context(|| format!("opening wlvl volume on {}", path.display()))?;
    if let Some(want) = cfg.uuid {
        if vol.uuid() != want {
            bail!(
                "{} holds volume {}, not {}",
                path.display(),
                vol.uuid(),
                want
            );
        }
    }
    let uuid = vol.uuid();
    if let Some(existing) = ublk_target::find_attached(uuid) {
        bail!(
            "volume {uuid} is already attached as {}",
            existing.bdev_path
        );
    }
    let repair = if vol.probe_report().mirror_ok && vol.probe_report().primary_ok {
        None
    } else {
        tracing::warn!(%uuid, "one superblock copy was unreadable; it will be repaired on next flush");
        Some(vol.superblock().clone())
    };

    let (dev, _sb, layout, ring, metadata) = vol.into_parts();
    let mapper = Arc::new(ChunkMapper::new(layout, metadata));
    let flush_signal = Arc::new(FlushSignal::default());

    // Buffered handle for metadata writes; the data-path handle becomes O_DIRECT.
    let meta_io = BackingDevice::open(&path, true)
        .with_context(|| format!("reopening {} for metadata", path.display()))?;
    let persister = Persister::new(meta_io, ring, mapper.clone(), repair);
    let persist_handle = persist::spawn(
        persister,
        flush_signal.clone(),
        PersistOptions {
            interval: flush_interval,
            ..Default::default()
        },
    );

    let wear_handle = if cfg.wear_leveling {
        let wear_io = BackingDevice::open(&path, true)
            .with_context(|| format!("reopening {} for migration", path.display()))?;
        let opts = WearOptions {
            check_interval: Duration::from_secs(cfg.check_interval_secs),
            rotate_interval: Duration::from_secs(cfg.rotate_interval_secs),
            policy: PolicyParams {
                wear_delta_threshold: cfg.wear_delta_threshold,
                retire_threshold: cfg.retire_threshold,
                ..Default::default()
            },
            ..Default::default()
        };
        Some(wear::spawn(
            mapper.clone(),
            wear_io,
            flush_signal.clone(),
            opts,
        ))
    } else {
        None
    };

    let live = Arc::new(LiveVolume {
        uuid,
        device: path.clone(),
        bdev: Mutex::new(None),
        layout,
        mapper: mapper.clone(),
        wear_stats: wear_handle.as_ref().map(|w| w.stats_arc()),
        flush_signal: flush_signal.clone(),
        health_params: HealthParams {
            expected_pe_cycles: cfg.expected_pe_cycles,
            ..Default::default()
        },
    });
    registry.add(live.clone());

    let target = Arc::new(
        Target::new(
            uuid,
            layout,
            mapper,
            flush_signal,
            &path,
            dev.into_file(),
            sector_shifts,
        )
        .with_verify_every(cfg.verify_every),
    );
    let opts = TargetOptions {
        dev_id: -1,
        nr_queues: cfg.queues,
        depth: cfg.depth,
        io_buf_bytes: cfg.io_buf_bytes,
    };

    let live_started = live.clone();
    let run_result = ublk_target::run(target, &opts, Some(ctrl_slot), move |ctrl| {
        let bdev = ctrl.get_bdev_path();
        *live_started.bdev.lock().unwrap_or_else(|e| e.into_inner()) = Some(bdev.clone());
        tracing::info!(%uuid, %bdev, "attached");
        on_started(&bdev);
    });

    // Teardown order: no new migrations, final metadata commit, then leave the control plane.
    if let Some(w) = wear_handle {
        w.shutdown();
    }
    persist_handle.shutdown();
    registry.remove(uuid);
    run_result?;
    tracing::info!(%uuid, "detached");
    Ok(())
}

/// Run every configured volume until SIGINT/SIGTERM.
pub fn run_daemon(mut config: Config, extra_uuids: &[Uuid]) -> Result<()> {
    config.merge_cli_uuids(extra_uuids);
    config.validate().map_err(|e| anyhow::anyhow!(e))?;
    if config.devices.is_empty() {
        bail!("no devices configured (add [[device]] entries or pass --uuid)");
    }
    if let Err(e) = ublk_target::unshare_mount_namespace() {
        tracing::warn!(error = %e, "could not enter a private mount namespace; do not mount /dev/ublkbN from this process tree");
    }

    let registry = Arc::new(Registry::default());
    let control_handle = control::serve(&config.global.control_socket, registry.clone())
        .with_context(|| {
            format!(
                "binding control socket {}",
                config.global.control_socket.display()
            )
        })?;
    tracing::info!(socket = %config.global.control_socket.display(), "control socket ready");

    let stopping = Arc::new(AtomicBool::new(false));
    let slots: Vec<CtrlSlot> = config.devices.iter().map(|_| new_ctrl_slot()).collect();
    {
        let (slots, stopping) = (slots.clone(), stopping.clone());
        ctrlc::set_handler(move || {
            if !stopping.swap(true, Ordering::SeqCst) {
                eprintln!("wlvl: shutting down");
            }
            for s in &slots {
                kill(s);
            }
        })
        .context("installing signal handler")?;
    }

    let flush = Duration::from_secs(config.global.metadata_flush_interval_secs);
    let mut threads = Vec::new();
    for (cfg, slot) in config.devices.iter().cloned().zip(slots.iter().cloned()) {
        let registry = registry.clone();
        let label = cfg.label();
        threads.push((
            label.clone(),
            std::thread::Builder::new()
                .name(format!("wlvl-vol-{label}"))
                .spawn(move || {
                    serve_volume(&cfg, flush, registry, slot, move |bdev| {
                        eprintln!("{label}: {bdev}")
                    })
                })
                .context("spawning volume thread")?,
        ));
    }

    let mut failures = 0;
    for (label, t) in threads {
        match t.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                failures += 1;
                tracing::error!(%label, error = %e, "volume failed");
            }
            Err(_) => {
                failures += 1;
                tracing::error!(%label, "volume thread panicked");
            }
        }
    }
    control_handle.shutdown();
    if failures > 0 && !stopping.load(Ordering::SeqCst) {
        bail!("{failures} volume(s) failed");
    }
    Ok(())
}
