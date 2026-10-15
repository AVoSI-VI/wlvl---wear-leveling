//! Metadata image: logical->physical mapping plus per-physical-chunk wear statistics.
//!
//! One image is written per commit into the metadata slot ring (see [`super::ring`]).

use super::codec::{crc, Reader, Writer};
use super::layout::Layout;
use super::{align_up, OnDiskError, Result, SECTOR_ALIGN};

pub const MAGIC: &[u8; 8] = b"WLMETA01";
pub const VERSION: u32 = 1;
pub const HEADER_BYTES: usize = 64;

/// Physical chunk state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkState {
    /// Available for allocation (spare).
    #[default]
    Free = 0,
    /// Currently mapped to a logical chunk.
    InUse = 1,
    /// Too many errors; never allocated again.
    Retired = 2,
    /// Superblock, mirror or metadata ring; never allocated.
    Reserved = 3,
}

impl ChunkState {
    fn from_u8(v: u8) -> Result<ChunkState> {
        Ok(match v {
            0 => ChunkState::Free,
            1 => ChunkState::InUse,
            2 => ChunkState::Retired,
            3 => ChunkState::Reserved,
            _ => {
                return Err(OnDiskError::Inconsistent(format!(
                    "unknown chunk state {v}"
                )))
            }
        })
    }
}

/// Wear and health counters for one physical chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ChunkStats {
    pub write_count: u64,
    pub io_errors: u16,
    pub verify_failures: u16,
    pub latency_ewma_us: u32,
    pub state: ChunkState,
}

impl ChunkStats {
    pub const ENCODED_LEN: usize = 24;

    fn encode(&self, w: &mut Writer) {
        w.u64(self.write_count)
            .u16(self.io_errors)
            .u16(self.verify_failures)
            .u32(self.latency_ewma_us)
            .u8(self.state as u8)
            .bytes(&[0u8; 7]);
    }

    fn decode(r: &mut Reader<'_>) -> Result<ChunkStats> {
        let s = ChunkStats {
            write_count: r.u64()?,
            io_errors: r.u16()?,
            verify_failures: r.u16()?,
            latency_ewma_us: r.u32()?,
            state: ChunkState::from_u8(r.u8()?)?,
        };
        r.skip(7)?;
        Ok(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub generation: u64,
    /// `mapping[logical] = physical`.
    pub mapping: Vec<u32>,
    /// One entry per physical chunk.
    pub stats: Vec<ChunkStats>,
}

impl Metadata {
    /// Identity mapping: logical `i` lives on physical `pool_start + i`; the rest of the pool is spare.
    pub fn initial(layout: &Layout) -> Metadata {
        let pool_start = layout.pool_start();
        let mapping: Vec<u32> = (0..layout.data_chunks)
            .map(|l| (pool_start + l) as u32)
            .collect();
        let mut stats = vec![ChunkStats::default(); layout.total_chunks as usize];
        for (p, s) in stats.iter_mut().enumerate() {
            s.state = if layout.is_reserved(p as u64) {
                ChunkState::Reserved
            } else {
                ChunkState::Free
            };
        }
        for &p in &mapping {
            stats[p as usize].state = ChunkState::InUse;
        }
        Metadata {
            generation: 0,
            mapping,
            stats,
        }
    }

    /// Unpadded payload length for a layout.
    pub fn payload_len(layout: &Layout) -> usize {
        layout.data_chunks as usize * 4 + layout.total_chunks as usize * ChunkStats::ENCODED_LEN
    }

    /// Full encoded image length (header + payload, padded to the sector alignment).
    pub fn image_len(layout: &Layout) -> usize {
        align_up(HEADER_BYTES + Metadata::payload_len(layout), SECTOR_ALIGN)
    }

    pub fn encode(&self) -> Vec<u8> {
        let payload_len = self.mapping.len() * 4 + self.stats.len() * ChunkStats::ENCODED_LEN;
        let mut w = Writer::with_capacity(align_up(HEADER_BYTES + payload_len, SECTOR_ALIGN));
        w.bytes(MAGIC)
            .u32(VERSION)
            .u32(0)
            .u64(self.generation)
            .u64(self.mapping.len() as u64)
            .u64(self.stats.len() as u64)
            .u32(payload_len as u32)
            .pad_to(HEADER_BYTES - 4)
            .u32(0); // crc placeholder
        for &p in &self.mapping {
            w.u32(p);
        }
        for s in &self.stats {
            s.encode(&mut w);
        }
        let c = {
            let buf = w.as_slice();
            let mut h = Vec::with_capacity(buf.len());
            h.extend_from_slice(&buf[..HEADER_BYTES - 4]);
            h.extend_from_slice(&buf[HEADER_BYTES..]);
            crc(&h)
        };
        w.as_mut_slice()[HEADER_BYTES - 4..HEADER_BYTES].copy_from_slice(&c.to_le_bytes());
        let total = align_up(w.len(), SECTOR_ALIGN);
        w.pad_to(total);
        w.into_vec()
    }

    /// Decode and validate an image against `layout`.
    pub fn decode(buf: &[u8], layout: &Layout, offset_for_error: u64) -> Result<Metadata> {
        if buf.len() < HEADER_BYTES || &buf[..8] != MAGIC {
            return Err(OnDiskError::BadMagic);
        }
        let mut r = Reader::new(&buf[..HEADER_BYTES]);
        r.skip(8)?;
        let version = r.u32()?;
        if version != VERSION {
            return Err(OnDiskError::UnsupportedVersion(version));
        }
        r.skip(4)?;
        let generation = r.u64()?;
        let data_chunks = r.u64()?;
        let total_chunks = r.u64()?;
        let payload_len = r.u32()? as usize;
        r.skip(HEADER_BYTES - 4 - r.position())?;
        let stored = r.u32()?;
        if data_chunks != layout.data_chunks || total_chunks != layout.total_chunks {
            return Err(OnDiskError::Inconsistent(
                "metadata chunk counts differ from superblock".into(),
            ));
        }
        if payload_len != Metadata::payload_len(layout) || buf.len() < HEADER_BYTES + payload_len {
            return Err(OnDiskError::Inconsistent(
                "metadata payload length mismatch".into(),
            ));
        }
        let payload = &buf[HEADER_BYTES..HEADER_BYTES + payload_len];
        let mut h = Vec::with_capacity(HEADER_BYTES + payload_len);
        h.extend_from_slice(&buf[..HEADER_BYTES - 4]);
        h.extend_from_slice(payload);
        if crc(&h) != stored {
            return Err(OnDiskError::BadCrc {
                offset: offset_for_error,
            });
        }
        let mut r = Reader::new(payload);
        let mut mapping = Vec::with_capacity(data_chunks as usize);
        for _ in 0..data_chunks {
            mapping.push(r.u32()?);
        }
        let mut stats = Vec::with_capacity(total_chunks as usize);
        for _ in 0..total_chunks {
            stats.push(ChunkStats::decode(&mut r)?);
        }
        let md = Metadata {
            generation,
            mapping,
            stats,
        };
        md.validate(layout)?;
        Ok(md)
    }

    /// Structural checks: mapping targets are in-pool, unique and marked InUse.
    pub fn validate(&self, layout: &Layout) -> Result<()> {
        let mut seen = vec![false; layout.total_chunks as usize];
        for (l, &p) in self.mapping.iter().enumerate() {
            let p = p as u64;
            if p >= layout.total_chunks || layout.is_reserved(p) {
                return Err(OnDiskError::Inconsistent(format!(
                    "logical {l} maps to invalid physical {p}"
                )));
            }
            if seen[p as usize] {
                return Err(OnDiskError::Inconsistent(format!(
                    "physical {p} mapped twice"
                )));
            }
            seen[p as usize] = true;
            if self.stats[p as usize].state != ChunkState::InUse {
                return Err(OnDiskError::Inconsistent(format!(
                    "mapped physical {p} is not InUse"
                )));
            }
        }
        for (p, s) in self.stats.iter().enumerate() {
            if s.state == ChunkState::InUse && !seen[p] {
                return Err(OnDiskError::Inconsistent(format!(
                    "physical {p} InUse but unmapped"
                )));
            }
        }
        Ok(())
    }

    pub fn physical_of(&self, lchunk: u64) -> u64 {
        self.mapping[lchunk as usize] as u64
    }

    pub fn free_chunks(&self) -> impl Iterator<Item = u64> + '_ {
        self.stats
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state == ChunkState::Free)
            .map(|(p, _)| p as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::layout::MIB;

    fn layout() -> Layout {
        Layout::compute(256 * MIB, 4 * MIB, 4, 10).unwrap()
    }

    #[test]
    fn initial_is_valid_and_roundtrips() {
        let l = layout();
        let md = Metadata::initial(&l);
        md.validate(&l).unwrap();
        assert_eq!(md.free_chunks().count() as u64, l.spare_chunks);
        let bytes = md.encode();
        assert_eq!(bytes.len(), Metadata::image_len(&l));
        assert_eq!(bytes.len() % SECTOR_ALIGN, 0);
        let back = Metadata::decode(&bytes, &l, 0).unwrap();
        assert_eq!(md, back);
    }

    #[test]
    fn corruption_detected() {
        let l = layout();
        let mut bytes = Metadata::initial(&l).encode();
        bytes[HEADER_BYTES + 3] ^= 1;
        assert!(matches!(
            Metadata::decode(&bytes, &l, 5),
            Err(OnDiskError::BadCrc { offset: 5 })
        ));
    }

    #[test]
    fn validate_rejects_duplicate_mapping() {
        let l = layout();
        let mut md = Metadata::initial(&l);
        md.mapping[1] = md.mapping[0];
        assert!(md.validate(&l).is_err());
    }
}
