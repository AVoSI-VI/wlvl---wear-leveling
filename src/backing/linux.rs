//! Linux-specific device helpers: block ioctls, sysfs scanning, mount table.
//!
//! The ioctl definitions follow libublk's `examples/loop.rs`.

use super::ScanEntry;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const BLK_IOCTL_TYPE: u8 = 0x12; // linux/fs.h
const BLKGETSIZE64_NR: u8 = 114;
const BLKSSZGET_NR: u8 = 104;
const BLKPBSZGET_NR: u8 = 123;

nix::ioctl_read!(ioctl_blkgetsize64, BLK_IOCTL_TYPE, BLKGETSIZE64_NR, u64);
nix::ioctl_read_bad!(
    ioctl_blksszget,
    nix::request_code_none!(BLK_IOCTL_TYPE, BLKSSZGET_NR),
    i32
);
nix::ioctl_read_bad!(
    ioctl_blkpbszget,
    nix::request_code_none!(BLK_IOCTL_TYPE, BLKPBSZGET_NR),
    u32
);

/// Device names (prefixes) that can never be a wear-leveling target.
const SKIP_PREFIXES: &[&str] = &[
    "loop", "dm-", "ublkb", "ublkc", "ram", "zram", "sr", "nbd", "md", "fd",
];

pub fn device_len(file: &File) -> io::Result<u64> {
    let meta = file.metadata()?;
    if meta.file_type().is_block_device() {
        let mut cap = 0u64;
        unsafe { ioctl_blkgetsize64(file.as_raw_fd(), &mut cap as *mut u64) }
            .map_err(io::Error::from)?;
        Ok(cap)
    } else {
        Ok(meta.len())
    }
}

/// `(logical_bs_shift, physical_bs_shift)`; files report 512/4096 like `loop.rs`.
pub fn sector_shifts(file: &File) -> io::Result<(u8, u8)> {
    if !file.metadata()?.file_type().is_block_device() {
        return Ok((9, 12));
    }
    let fd = file.as_raw_fd();
    let mut ssz = 0i32;
    let mut pbsz = 0u32;
    unsafe {
        ioctl_blksszget(fd, &mut ssz as *mut i32).map_err(io::Error::from)?;
        ioctl_blkpbszget(fd, &mut pbsz as *mut u32).map_err(io::Error::from)?;
    }
    if ssz <= 0 || pbsz == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "device reported zero sector size",
        ));
    }
    Ok((ssz.trailing_zeros() as u8, pbsz.trailing_zeros() as u8))
}

/// Every `/sys/class/block` entry (disks and partitions) that holds a wlvl superblock.
pub fn scan_block_devices() -> io::Result<Vec<ScanEntry>> {
    let aliases = by_id_aliases();
    let mut out = Vec::new();
    for entry in fs::read_dir("/sys/class/block")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let path = PathBuf::from("/dev").join(&*name);
        if let Some(mut e) = ScanEntry::probe(&path) {
            if let Some(a) = aliases.get(&path) {
                e.aliases = a.clone();
            }
            out.push(e);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Map `/dev/sdX` -> all `/dev/disk/by-id/*` links pointing at it.
fn by_id_aliases() -> HashMap<PathBuf, Vec<PathBuf>> {
    let mut map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    if let Ok(rd) = fs::read_dir("/dev/disk/by-id") {
        for entry in rd.flatten() {
            let link = entry.path();
            if let Ok(target) = fs::canonicalize(&link) {
                map.entry(target).or_default().push(link);
            }
        }
    }
    for v in map.values_mut() {
        v.sort();
    }
    map
}

/// `true` if the device or one of its partitions appears in `/proc/mounts`.
pub fn is_mounted(path: &Path) -> io::Result<bool> {
    let target = fs::canonicalize(path)?;
    let target_str = target.to_string_lossy().into_owned();
    let mounts = fs::read_to_string("/proc/mounts")?;
    for line in mounts.lines() {
        let Some(src) = line.split_whitespace().next() else {
            continue;
        };
        if !src.starts_with("/dev/") {
            continue;
        }
        let Ok(src) = fs::canonicalize(src) else {
            continue;
        };
        let src = src.to_string_lossy();
        if *src == *target_str {
            return Ok(true);
        }
        if let Some(rest) = src.strip_prefix(target_str.as_str()) {
            let rest = rest.strip_prefix('p').unwrap_or(rest);
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
