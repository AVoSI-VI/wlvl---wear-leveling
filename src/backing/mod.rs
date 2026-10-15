//! Access to the raw backing device (SD card, USB stick, or a plain file for tests).

#[cfg(target_os = "linux")]
pub mod linux;

use crate::ondisk::{BlockIo, Superblock};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// An opened backing device with its size resolved up front
/// (block devices report `len() == 0` through `metadata()`).
pub struct BackingDevice {
    file: File,
    len: u64,
    path: PathBuf,
}

impl BackingDevice {
    pub fn open(path: &Path, write: bool) -> io::Result<BackingDevice> {
        let file = OpenOptions::new().read(true).write(write).open(path)?;
        let len = device_len(&file)?;
        Ok(BackingDevice {
            file,
            len,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn into_file(self) -> File {
        self.file
    }

    /// Logical and physical sector sizes (shift values, e.g. 9 for 512 bytes).
    pub fn sector_shifts(&self) -> io::Result<(u8, u8)> {
        sector_shifts(&self.file)
    }
}

impl BlockIo for BackingDevice {
    fn len(&self) -> io::Result<u64> {
        Ok(self.len)
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_at(buf, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.file.sync()
    }
}

/// A formatted volume found during a scan.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanEntry {
    pub path: PathBuf,
    pub uuid: Uuid,
    pub chunk_size: u64,
    pub total_chunks: u64,
    pub data_chunks: u64,
    pub exported_bytes: u64,
    pub primary_ok: bool,
    pub mirror_ok: bool,
    /// Stable aliases such as `/dev/disk/by-id/...`.
    pub aliases: Vec<PathBuf>,
}

impl ScanEntry {
    /// Probe one path; `None` if it does not hold a wlvl superblock.
    pub fn probe(path: &Path) -> Option<ScanEntry> {
        let dev = BackingDevice::open(path, false).ok()?;
        let (sb, report) = Superblock::probe(&dev).ok()?;
        let layout = sb.layout().ok()?;
        Some(ScanEntry {
            path: path.to_path_buf(),
            uuid: sb.uuid,
            chunk_size: sb.chunk_size,
            total_chunks: sb.total_chunks,
            data_chunks: sb.data_chunks,
            exported_bytes: layout.exported_bytes(),
            primary_ok: report.primary_ok,
            mirror_ok: report.mirror_ok,
            aliases: Vec::new(),
        })
    }
}

/// Scan the system's block devices for wlvl volumes.
pub fn scan() -> io::Result<Vec<ScanEntry>> {
    #[cfg(target_os = "linux")]
    {
        linux::scan_block_devices()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "device scanning is only supported on Linux",
        ))
    }
}

/// Resolve a configured identifier (UUID, or a device path) to a device path.
pub fn resolve(uuid: Option<Uuid>, path: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(p) = path {
        return Ok(p.to_path_buf());
    }
    let uuid =
        uuid.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "need a uuid or a path"))?;
    let found: Vec<ScanEntry> = scan()?.into_iter().filter(|e| e.uuid == uuid).collect();
    match found.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no device holds wlvl volume {uuid}"),
        )),
        1 => Ok(found.into_iter().next().unwrap().path),
        n => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{n} devices hold wlvl volume {uuid}"),
        )),
    }
}

/// `true` if `path` (or any partition of it) appears in the mount table.
pub fn is_mounted(path: &Path) -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        linux::is_mounted(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(false)
    }
}

fn device_len(file: &File) -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        linux::device_len(file)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(file.metadata()?.len())
    }
}

fn sector_shifts(file: &File) -> io::Result<(u8, u8)> {
    #[cfg(target_os = "linux")]
    {
        linux::sector_shifts(file)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        Ok((9, 12))
    }
}
