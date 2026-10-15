//! Superblock: identity and geometry of a formatted device. Written only by `format`.

use super::codec::{crc, Reader, Writer};
use super::layout::{Layout, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE};
use super::{BlockIo, OnDiskError, Result, SECTOR_ALIGN};
use uuid::Uuid;

pub const MAGIC: &[u8; 8] = b"WLSUPER1";
pub const VERSION: u32 = 1;
pub const SUPERBLOCK_BYTES: usize = SECTOR_ALIGN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub uuid: Uuid,
    pub flags: u32,
    pub chunk_size: u64,
    pub total_chunks: u64,
    pub data_chunks: u64,
    pub spare_chunks: u64,
    pub meta_ring_start: u64,
    pub meta_slots: u32,
    pub created_unix_secs: u64,
}

/// Which copies were readable during [`Superblock::probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeReport {
    pub primary_ok: bool,
    pub mirror_ok: bool,
}

impl Superblock {
    pub fn new(layout: &Layout, uuid: Uuid) -> Superblock {
        let created_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Superblock {
            uuid,
            flags: 0,
            chunk_size: layout.chunk_size,
            total_chunks: layout.total_chunks,
            data_chunks: layout.data_chunks,
            spare_chunks: layout.spare_chunks,
            meta_ring_start: layout.meta_slot_chunk(0),
            meta_slots: layout.meta_slots,
            created_unix_secs,
        }
    }

    pub fn layout(&self) -> Result<Layout> {
        if self.meta_ring_start != 1 {
            return Err(OnDiskError::Inconsistent(
                "metadata ring must start at chunk 1".into(),
            ));
        }
        Layout::from_parts(
            self.chunk_size,
            self.total_chunks,
            self.meta_slots,
            self.spare_chunks,
            self.data_chunks,
        )
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(SUPERBLOCK_BYTES);
        w.bytes(MAGIC)
            .u32(VERSION)
            .u32(self.flags)
            .bytes(self.uuid.as_bytes())
            .u64(self.chunk_size)
            .u64(self.total_chunks)
            .u64(self.data_chunks)
            .u64(self.spare_chunks)
            .u64(self.meta_ring_start)
            .u32(self.meta_slots)
            .u64(self.created_unix_secs)
            .pad_to(SUPERBLOCK_BYTES - 4);
        let c = crc(w.as_slice());
        w.u32(c);
        w.into_vec()
    }

    pub fn decode(buf: &[u8], offset_for_error: u64) -> Result<Superblock> {
        if buf.len() < SUPERBLOCK_BYTES {
            return Err(OnDiskError::Inconsistent(
                "superblock buffer too small".into(),
            ));
        }
        let body = &buf[..SUPERBLOCK_BYTES - 4];
        let stored = u32::from_le_bytes(
            buf[SUPERBLOCK_BYTES - 4..SUPERBLOCK_BYTES]
                .try_into()
                .unwrap(),
        );
        if &body[..8] != MAGIC {
            return Err(OnDiskError::BadMagic);
        }
        if crc(body) != stored {
            return Err(OnDiskError::BadCrc {
                offset: offset_for_error,
            });
        }
        let mut r = Reader::new(body);
        r.skip(8)?;
        let version = r.u32()?;
        if version != VERSION {
            return Err(OnDiskError::UnsupportedVersion(version));
        }
        let flags = r.u32()?;
        let uuid = Uuid::from_bytes(r.array::<16>()?);
        let sb = Superblock {
            uuid,
            flags,
            chunk_size: r.u64()?,
            total_chunks: r.u64()?,
            data_chunks: r.u64()?,
            spare_chunks: r.u64()?,
            meta_ring_start: r.u64()?,
            meta_slots: r.u32()?,
            created_unix_secs: r.u64()?,
        };
        sb.layout()?;
        Ok(sb)
    }

    /// Write primary and mirror copies.
    pub fn write_both<B: BlockIo>(&self, io: &B) -> Result<()> {
        let layout = self.layout()?;
        let bytes = self.encode();
        io.write_at(&bytes, layout.superblock_offset())?;
        io.write_at(&bytes, layout.mirror_offset())?;
        io.sync()?;
        Ok(())
    }

    fn read_at<B: BlockIo>(io: &B, offset: u64) -> Result<Superblock> {
        let mut buf = vec![0u8; SUPERBLOCK_BYTES];
        io.read_at(&mut buf, offset)?;
        Superblock::decode(&buf, offset)
    }

    /// Locate a superblock, falling back to the mirror when the primary is damaged.
    pub fn probe<B: BlockIo>(io: &B) -> Result<(Superblock, ProbeReport)> {
        let device_len = io.len()?;
        if device_len < SUPERBLOCK_BYTES as u64 {
            return Err(OnDiskError::NoSuperblock);
        }
        match Superblock::read_at(io, 0) {
            Ok(primary) => {
                let mirror_ok = primary
                    .layout()
                    .ok()
                    .and_then(|l| Superblock::read_at(io, l.mirror_offset()).ok())
                    .is_some_and(|m| m == primary);
                Ok((
                    primary,
                    ProbeReport {
                        primary_ok: true,
                        mirror_ok,
                    },
                ))
            }
            Err(OnDiskError::Io(e)) => Err(OnDiskError::Io(e)),
            Err(_) => {
                let mut cs = MIN_CHUNK_SIZE;
                while cs <= MAX_CHUNK_SIZE {
                    let total = device_len / cs;
                    if total >= 2 {
                        let off = (total - 1) * cs;
                        if let Ok(m) = Superblock::read_at(io, off) {
                            if m.chunk_size == cs && m.total_chunks == total {
                                return Ok((
                                    m,
                                    ProbeReport {
                                        primary_ok: false,
                                        mirror_ok: true,
                                    },
                                ));
                            }
                        }
                    }
                    cs <<= 1;
                }
                Err(OnDiskError::NoSuperblock)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::layout::MIB;

    #[test]
    fn encode_decode_roundtrip() {
        let layout = Layout::compute(1024 * MIB, 4 * MIB, 8, 5).unwrap();
        let sb = Superblock::new(&layout, Uuid::new_v4());
        let bytes = sb.encode();
        assert_eq!(bytes.len(), SUPERBLOCK_BYTES);
        let back = Superblock::decode(&bytes, 0).unwrap();
        assert_eq!(sb, back);
        assert_eq!(back.layout().unwrap(), layout);
    }

    #[test]
    fn detects_corruption() {
        let layout = Layout::compute(1024 * MIB, 4 * MIB, 8, 5).unwrap();
        let mut bytes = Superblock::new(&layout, Uuid::new_v4()).encode();
        bytes[40] ^= 0xff;
        assert!(matches!(
            Superblock::decode(&bytes, 7),
            Err(OnDiskError::BadCrc { offset: 7 })
        ));
        bytes[0] = b'X';
        assert!(matches!(
            Superblock::decode(&bytes, 0),
            Err(OnDiskError::BadMagic)
        ));
    }
}
