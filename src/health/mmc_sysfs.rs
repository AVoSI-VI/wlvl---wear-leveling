//! Health and identity from the Linux MMC subsystem (`/sys/class/block/mmcblkN/device`).
//!
//! `life_time` and `pre_eol_info` exist for eMMC 5.0+ only; SD cards expose
//! identity (CID fields) but no wear information.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmcReport {
    pub available: bool,
    pub source: String,
    pub sysfs_path: Option<PathBuf>,
    pub card_type: Option<String>,
    pub name: Option<String>,
    pub manfid: Option<String>,
    pub oemid: Option<String>,
    pub serial: Option<String>,
    pub date: Option<String>,
    /// eMMC device life time estimation type A/B: 1 = 0-10% used ... 10 = 90-100%, 11 = exceeded.
    pub life_time_a: Option<u8>,
    pub life_time_b: Option<u8>,
    /// eMMC pre-EOL info: normal / warning / urgent.
    pub pre_eol: Option<String>,
    pub note: Option<String>,
}

/// Locate the card directory for a block device path like `/dev/mmcblk0` or `/dev/mmcblk0p1`.
pub fn sysfs_card_dir(dev: &Path) -> Option<PathBuf> {
    let name = dev.file_name()?.to_str()?;
    if !name.starts_with("mmcblk") {
        return None;
    }
    // Strip a partition suffix (mmcblk0p1 -> mmcblk0).
    let base = match name.find('p') {
        Some(i) if i > "mmcblk".len() => &name[..i],
        _ => name,
    };
    let dir = PathBuf::from("/sys/class/block").join(base).join("device");
    dir.exists().then_some(dir)
}

pub fn query(dev: &Path) -> MmcReport {
    let mut r = MmcReport {
        source: "mmc sysfs".into(),
        ..Default::default()
    };
    let Some(dir) = sysfs_card_dir(dev) else {
        r.note = Some("not an mmcblk device (USB card readers expose no card registers)".into());
        return r;
    };
    r.available = true;
    r.sysfs_path = Some(dir.clone());
    r.card_type = read(&dir, "type");
    r.name = read(&dir, "name");
    r.manfid = read(&dir, "manfid");
    r.oemid = read(&dir, "oemid");
    r.serial = read(&dir, "serial");
    r.date = read(&dir, "date");
    if let Some(lt) = read(&dir, "life_time") {
        let (a, b) = parse_life_time(&lt);
        r.life_time_a = a;
        r.life_time_b = b;
    }
    r.pre_eol = read(&dir, "pre_eol_info").map(|v| pre_eol_label(&v).to_string());
    if r.life_time_a.is_none() && r.pre_eol.is_none() {
        r.note = Some("card exposes no life_time/pre_eol_info (SD cards do not)".into());
    }
    r
}

fn read(dir: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(file))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `life_time` is two hex bytes, e.g. "0x01 0x02".
pub fn parse_life_time(s: &str) -> (Option<u8>, Option<u8>) {
    let mut it = s.split_whitespace().map(parse_hex);
    (it.next().flatten(), it.next().flatten())
}

fn parse_hex(s: &str) -> Option<u8> {
    u8::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

pub fn pre_eol_label(s: &str) -> &'static str {
    match parse_hex(s.trim()) {
        Some(0x01) => "normal",
        Some(0x02) => "warning",
        Some(0x03) => "urgent",
        Some(0x00) => "not defined",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registers() {
        assert_eq!(parse_life_time("0x01 0x0a"), (Some(1), Some(10)));
        assert_eq!(parse_life_time("0x0B"), (Some(11), None));
        assert_eq!(parse_life_time("garbage"), (None, None));
        assert_eq!(pre_eol_label("0x01"), "normal");
        assert_eq!(pre_eol_label("0x03\n"), "urgent");
        assert_eq!(pre_eol_label("zz"), "unknown");
    }

    #[test]
    fn non_mmc_devices_are_unavailable() {
        assert!(sysfs_card_dir(Path::new("/dev/sdb")).is_none());
        let r = query(Path::new("/dev/sdb1"));
        assert!(!r.available);
        assert!(r.note.is_some());
    }
}
