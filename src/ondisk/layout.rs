//! Geometry of a formatted device.
//!
//! ```text
//! physical chunk index
//! [0]                  superblock
//! [1 .. 1+meta_slots)  metadata slot ring
//! [pool_start .. last) pool = spare chunks + data chunks (any order, decided by the mapping)
//! [last]               superblock mirror
//! ```

use super::{OnDiskError, Result};

pub const MIB: u64 = 1024 * 1024;
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * MIB;
pub const MIN_CHUNK_SIZE: u64 = 64 * 1024;
pub const MAX_CHUNK_SIZE: u64 = 1024 * MIB;
pub const DEFAULT_META_SLOTS: u32 = 8;
pub const MIN_META_SLOTS: u32 = 2;
pub const DEFAULT_SPARE_PCT: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub chunk_size: u64,
    pub total_chunks: u64,
    pub meta_slots: u32,
    pub spare_chunks: u64,
    pub data_chunks: u64,
}

impl Layout {
    /// Derive a layout for a device of `device_bytes`.
    pub fn compute(
        device_bytes: u64,
        chunk_size: u64,
        meta_slots: u32,
        spare_pct: u8,
    ) -> Result<Layout> {
        if !chunk_size.is_power_of_two() || !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size)
        {
            return Err(OnDiskError::InvalidLayout(format!(
                "chunk size {chunk_size} must be a power of two between {MIN_CHUNK_SIZE} and {MAX_CHUNK_SIZE}"
            )));
        }
        if meta_slots < MIN_META_SLOTS {
            return Err(OnDiskError::InvalidLayout(format!(
                "metadata slots {meta_slots} must be at least {MIN_META_SLOTS}"
            )));
        }
        if spare_pct > 50 {
            return Err(OnDiskError::InvalidLayout(format!(
                "spare percentage {spare_pct} exceeds 50"
            )));
        }
        let total_chunks = device_bytes / chunk_size;
        let reserved = 2 + meta_slots as u64;
        if total_chunks < reserved + 2 {
            return Err(OnDiskError::InvalidLayout(format!(
                "device holds {total_chunks} chunks of {chunk_size} bytes; need at least {}",
                reserved + 2
            )));
        }
        let pool = total_chunks - reserved;
        let spare_chunks = (pool * spare_pct as u64 / 100).max(1);
        let data_chunks = pool - spare_chunks;
        if data_chunks == 0 {
            return Err(OnDiskError::InvalidLayout(
                "no data chunks left after reserving spares".into(),
            ));
        }
        Ok(Layout {
            chunk_size,
            total_chunks,
            meta_slots,
            spare_chunks,
            data_chunks,
        })
    }

    /// Rebuild a layout from values stored in the superblock, validating consistency.
    pub fn from_parts(
        chunk_size: u64,
        total_chunks: u64,
        meta_slots: u32,
        spare_chunks: u64,
        data_chunks: u64,
    ) -> Result<Layout> {
        let l = Layout {
            chunk_size,
            total_chunks,
            meta_slots,
            spare_chunks,
            data_chunks,
        };
        let expected_pool = total_chunks.checked_sub(2 + meta_slots as u64);
        if !chunk_size.is_power_of_two()
            || expected_pool != Some(spare_chunks + data_chunks)
            || data_chunks == 0
        {
            return Err(OnDiskError::Inconsistent(
                "superblock geometry does not add up".into(),
            ));
        }
        Ok(l)
    }

    pub fn superblock_offset(&self) -> u64 {
        0
    }

    pub fn mirror_chunk(&self) -> u64 {
        self.total_chunks - 1
    }

    pub fn mirror_offset(&self) -> u64 {
        self.chunk_offset(self.mirror_chunk())
    }

    pub fn meta_slot_chunk(&self, slot: u32) -> u64 {
        1 + slot as u64
    }

    pub fn meta_slot_offset(&self, slot: u32) -> u64 {
        self.chunk_offset(self.meta_slot_chunk(slot))
    }

    /// First physical chunk of the pool (data + spare).
    pub fn pool_start(&self) -> u64 {
        1 + self.meta_slots as u64
    }

    pub fn pool_chunks(&self) -> u64 {
        self.spare_chunks + self.data_chunks
    }

    pub fn chunk_offset(&self, pchunk: u64) -> u64 {
        pchunk * self.chunk_size
    }

    /// Size of the block device exported to the kernel.
    pub fn exported_bytes(&self) -> u64 {
        self.data_chunks * self.chunk_size
    }

    /// `true` for chunks that must never appear in the mapping.
    pub fn is_reserved(&self, pchunk: u64) -> bool {
        pchunk < self.pool_start() || pchunk >= self.mirror_chunk()
    }

    /// Split a byte range of the exported device into (logical chunk, offset-in-chunk, len) pieces.
    pub fn split_range(&self, offset: u64, len: u64) -> Vec<(u64, u64, u64)> {
        let mut out = Vec::with_capacity(2);
        let mut off = offset;
        let end = offset + len;
        while off < end {
            let lchunk = off / self.chunk_size;
            let in_chunk = off % self.chunk_size;
            let n = (self.chunk_size - in_chunk).min(end - off);
            out.push((lchunk, in_chunk, n));
            off += n;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_basic() {
        let l = Layout::compute(1024 * MIB, 4 * MIB, 8, 5).unwrap();
        assert_eq!(l.total_chunks, 256);
        assert_eq!(l.pool_start(), 9);
        assert_eq!(l.pool_chunks(), 246);
        assert_eq!(l.spare_chunks, 12);
        assert_eq!(l.data_chunks, 234);
        assert_eq!(l.mirror_chunk(), 255);
        assert!(l.is_reserved(0) && l.is_reserved(8) && l.is_reserved(255));
        assert!(!l.is_reserved(9) && !l.is_reserved(254));
        assert_eq!(l.exported_bytes(), 234 * 4 * MIB);
    }

    #[test]
    fn rejects_bad_params() {
        assert!(Layout::compute(1024 * MIB, 3 * MIB, 8, 5).is_err());
        assert!(Layout::compute(1024 * MIB, 4 * MIB, 1, 5).is_err());
        assert!(Layout::compute(1024 * MIB, 4 * MIB, 8, 51).is_err());
        assert!(Layout::compute(8 * MIB, 4 * MIB, 8, 5).is_err());
    }

    #[test]
    fn spare_minimum_is_one() {
        let l = Layout::compute(64 * MIB, 4 * MIB, 2, 0).unwrap();
        assert_eq!(l.spare_chunks, 1);
        assert_eq!(l.data_chunks, 11);
    }

    #[test]
    fn from_parts_roundtrip() {
        let l = Layout::compute(1024 * MIB, 4 * MIB, 8, 5).unwrap();
        let r = Layout::from_parts(
            l.chunk_size,
            l.total_chunks,
            l.meta_slots,
            l.spare_chunks,
            l.data_chunks,
        )
        .unwrap();
        assert_eq!(l, r);
        assert!(Layout::from_parts(
            l.chunk_size,
            l.total_chunks,
            l.meta_slots,
            l.spare_chunks + 1,
            l.data_chunks
        )
        .is_err());
    }

    #[test]
    fn split_range_crosses_boundary() {
        let l = Layout::compute(1024 * MIB, 4 * MIB, 8, 5).unwrap();
        assert_eq!(l.split_range(0, 4096), vec![(0, 0, 4096)]);
        assert_eq!(
            l.split_range(4 * MIB - 1024, 4096),
            vec![(0, 4 * MIB - 1024, 1024), (1, 0, 3072)]
        );
        assert!(l.split_range(0, 0).is_empty());
    }
}
