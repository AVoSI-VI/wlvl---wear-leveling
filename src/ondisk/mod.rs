//! On-disk format: superblock, chunk layout and the metadata slot ring.
//!
//! Everything in this module is OS-independent and operates on a [`BlockIo`]
//! implementor, so the format can be exercised against a plain file in tests.

pub mod codec;
pub mod layout;
pub mod metadata;
pub mod ring;
pub mod superblock;
pub mod volume;

use std::fmt;
use std::io;

pub use layout::Layout;
pub use metadata::{ChunkState, ChunkStats, Metadata};
pub use ring::MetaRing;
pub use superblock::Superblock;
pub use volume::{FormatOptions, Volume};

/// Alignment used for every on-disk structure so O_DIRECT works on 4 KiB-sector devices.
pub const SECTOR_ALIGN: usize = 4096;

/// Positional block I/O over a device or a file.
#[allow(clippy::len_without_is_empty)]
pub trait BlockIo {
    /// Total length in bytes.
    fn len(&self) -> io::Result<u64>;
    /// Read exactly `buf.len()` bytes at `offset`.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// Write all of `buf` at `offset`.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Flush device caches.
    fn sync(&self) -> io::Result<()>;
}

impl BlockIo for std::fs::File {
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    #[cfg(unix)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buf, offset)
    }

    #[cfg(unix)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(self, buf, offset)
    }

    #[cfg(windows)]
    fn read_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            let n = self.seek_read(buf, offset)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
            }
            buf = &mut buf[n..];
            offset += n as u64;
        }
        Ok(())
    }

    #[cfg(windows)]
    fn write_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            let n = self.seek_write(buf, offset)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
            }
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.sync_data()
    }
}

/// Errors produced while reading or writing the on-disk format.
#[derive(Debug)]
pub enum OnDiskError {
    Io(io::Error),
    /// Magic bytes did not match.
    BadMagic,
    /// CRC mismatch; carries the offset of the structure.
    BadCrc {
        offset: u64,
    },
    /// Format version is not understood.
    UnsupportedVersion(u32),
    /// The layout parameters are invalid; message explains why.
    InvalidLayout(String),
    /// Neither superblock copy could be read.
    NoSuperblock,
    /// No valid metadata image exists in the ring.
    NoMetadata,
    /// Structure fields disagree with the superblock/layout.
    Inconsistent(String),
    /// `format` refused because a valid volume with this UUID already exists.
    AlreadyFormatted(uuid::Uuid),
}

impl fmt::Display for OnDiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnDiskError::Io(e) => write!(f, "I/O error: {e}"),
            OnDiskError::BadMagic => write!(f, "bad magic"),
            OnDiskError::BadCrc { offset } => write!(f, "CRC mismatch at offset {offset}"),
            OnDiskError::UnsupportedVersion(v) => write!(f, "unsupported format version {v}"),
            OnDiskError::InvalidLayout(m) => write!(f, "invalid layout: {m}"),
            OnDiskError::NoSuperblock => write!(f, "no valid wlvl superblock found"),
            OnDiskError::NoMetadata => write!(f, "no valid metadata image found"),
            OnDiskError::Inconsistent(m) => write!(f, "inconsistent on-disk data: {m}"),
            OnDiskError::AlreadyFormatted(u) => {
                write!(f, "device already holds wlvl volume {u} (use --force)")
            }
        }
    }
}

impl std::error::Error for OnDiskError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OnDiskError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for OnDiskError {
    fn from(e: io::Error) -> Self {
        OnDiskError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, OnDiskError>;

/// Round `n` up to a multiple of `align`.
pub(crate) fn align_up(n: usize, align: usize) -> usize {
    n.div_ceil(align) * align
}
