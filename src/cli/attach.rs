//! `wlvl attach` / `wlvl detach` (Linux only). A thin wrapper over `daemon::serve_volume`.

use super::{AttachArgs, DaemonArgs, DetachArgs};
use crate::config::DeviceConfig;
use crate::control::{self, Registry};
use crate::daemon;
use crate::ublk_target;
use anyhow::{bail, Context, Result};
use std::sync::Arc;
use std::time::Duration;

pub fn attach(a: AttachArgs) -> Result<()> {
    let mut cfg = match a.uuid {
        Some(u) => DeviceConfig::for_uuid(u),
        None => DeviceConfig {
            uuid: None,
            ..DeviceConfig::for_uuid(uuid::Uuid::nil())
        },
    };
    cfg.path = a.device.clone();
    cfg.check_interval_secs = a.check_interval_secs;
    cfg.rotate_interval_secs = a.rotate_interval_secs;
    cfg.wear_delta_threshold = a.wear_delta_threshold;
    cfg.wear_leveling = !a.no_wear_leveling;
    cfg.expected_pe_cycles = a.expected_pe_cycles;
    cfg.verify_every = a.verify_every;
    cfg.queues = a.queues;
    cfg.depth = a.depth;
    cfg.io_buf_bytes = a.io_buf_bytes;

    // Resolve the UUID up front so the per-volume socket name is known.
    let uuid = match a.uuid {
        Some(u) => u,
        None => {
            let path = a.device.as_deref().context("need --uuid or --device")?;
            crate::backing::ScanEntry::probe(path)
                .with_context(|| format!("{} holds no wlvl volume", path.display()))?
                .uuid
        }
    };
    cfg.uuid = Some(uuid);

    if !a.foreground {
        // Keep stdio until the device path has been printed.
        nix::unistd::daemon(true, true).context("daemonizing")?;
    }
    if let Err(e) = ublk_target::unshare_mount_namespace() {
        tracing::warn!(error = %e, "could not enter a private mount namespace; do not mount /dev/ublkbN from this process tree");
    }

    let registry = Arc::new(Registry::default());
    let sock_path = a
        .control_socket
        .clone()
        .unwrap_or_else(|| control::per_volume_socket(uuid));
    let control_handle = match control::serve(&sock_path, registry.clone()) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(socket = %sock_path.display(), error = %e, "control socket unavailable; `wlvl status` will read the device offline");
            None
        }
    };

    let slot = daemon::new_ctrl_slot();
    install_ctrlc(slot.clone());
    let foreground = a.foreground;
    let result = daemon::serve_volume(
        &cfg,
        Duration::from_secs(a.flush_interval_secs),
        registry,
        slot,
        move |bdev| {
            println!("{bdev}");
            if !foreground {
                redirect_stdio_to_null();
            }
        },
    );
    if let Some(c) = control_handle {
        c.shutdown();
    }
    result
}

pub fn daemon(a: DaemonArgs) -> Result<()> {
    let config = match crate::config::Config::load(&a.config) {
        Ok(c) => c,
        Err(e) if a.ignore_missing_config && !a.config.exists() => {
            tracing::warn!(path = %a.config.display(), "config file missing; serving --uuid volumes only ({e})");
            crate::config::Config::default()
        }
        Err(e) => bail!("{e}"),
    };
    daemon::run_daemon(config, &a.uuids)
}

pub fn detach(a: DetachArgs) -> Result<()> {
    let dev_id = match (a.dev_id, a.uuid) {
        (Some(id), _) => id,
        (None, Some(uuid)) => {
            ublk_target::find_attached(uuid)
                .with_context(|| format!("volume {uuid} is not attached"))?
                .dev_id
        }
        (None, None) => bail!("need --uuid or --dev-id"),
    };
    ublk_target::delete_device(dev_id)?;
    eprintln!("deleted /dev/ublkb{dev_id}");
    Ok(())
}

fn install_ctrlc(slot: daemon::CtrlSlot) {
    if let Err(e) = ctrlc::set_handler(move || daemon::kill(&slot)) {
        tracing::warn!(error = %e, "could not install signal handler");
    }
}

fn redirect_stdio_to_null() {
    use std::os::unix::io::AsRawFd;
    if let Ok(null) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        for fd in 0..3 {
            // SAFETY: dup2 onto the standard descriptors of our own process.
            unsafe {
                libc::dup2(null.as_raw_fd(), fd);
            }
        }
    }
}
