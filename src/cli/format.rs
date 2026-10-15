//! `wlvl format`

use super::FormatArgs;
use crate::backing::{self, BackingDevice};
use crate::ondisk::{FormatOptions, Volume};
use anyhow::{bail, Context, Result};

pub fn run(a: FormatArgs) -> Result<()> {
    if !a.yes {
        bail!(
            "refusing to format {} without --yes (this destroys all data on it)",
            a.device.display()
        );
    }
    if backing::is_mounted(&a.device).unwrap_or(false) {
        bail!(
            "{} (or a partition of it) is mounted; unmount first",
            a.device.display()
        );
    }
    let dev = BackingDevice::open(&a.device, true)
        .with_context(|| format!("opening {} for writing", a.device.display()))?;
    let opts = FormatOptions {
        chunk_size: a.chunk_size,
        meta_slots: a.meta_slots,
        spare_pct: a.spare_pct,
        uuid: a.uuid,
        force: a.force,
    };
    let vol =
        Volume::format(dev, &opts).with_context(|| format!("formatting {}", a.device.display()))?;
    let l = vol.layout();
    println!("{}", vol.uuid());
    eprintln!(
        "formatted {}: {} chunks of {} bytes, {} data, {} spare, {} metadata slots, exported size {} bytes",
        a.device.display(),
        l.total_chunks,
        l.chunk_size,
        l.data_chunks,
        l.spare_chunks,
        l.meta_slots,
        l.exported_bytes()
    );
    Ok(())
}
