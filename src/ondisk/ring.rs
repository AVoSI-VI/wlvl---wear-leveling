//! Metadata slot ring: each commit writes a whole image into the next slot.
//!
//! Recovery picks the slot with the highest valid generation, so a torn write
//! only loses the newest commit, never the previous good one. Rotating through
//! slots also spreads the metadata writes across several chunks.

use super::layout::Layout;
use super::metadata::Metadata;
use super::{BlockIo, OnDiskError, Result, SECTOR_ALIGN};

#[derive(Debug, Clone)]
pub struct MetaRing {
    layout: Layout,
    /// Slot the next commit will be written to.
    next_slot: u32,
    /// Generation of the newest image on disk; the ring owns this counter so
    /// callers may commit clones of the metadata.
    generation: u64,
}

impl MetaRing {
    /// Ring positioned so the first commit goes to slot 0. Used right after `format`.
    pub fn fresh(layout: Layout) -> MetaRing {
        MetaRing {
            layout,
            next_slot: 0,
            generation: 0,
        }
    }

    pub fn next_slot(&self) -> u32 {
        self.next_slot
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn slots(&self) -> u32 {
        self.layout.meta_slots
    }

    /// Scan all slots and return the newest valid image together with a ring
    /// positioned to write the slot after it.
    pub fn load<B: BlockIo>(io: &B, layout: Layout) -> Result<(MetaRing, Metadata)> {
        let image_len = Metadata::image_len(&layout);
        let mut best: Option<(u32, Metadata)> = None;
        let mut first_io_err: Option<std::io::Error> = None;
        let mut buf = vec![0u8; image_len];
        for slot in 0..layout.meta_slots {
            let off = layout.meta_slot_offset(slot);
            if let Err(e) = io.read_at(&mut buf, off) {
                tracing::warn!(slot, error = %e, "metadata slot unreadable");
                first_io_err.get_or_insert(e);
                continue;
            }
            match Metadata::decode(&buf, &layout, off) {
                Ok(md) => {
                    if best
                        .as_ref()
                        .is_none_or(|(_, b)| md.generation > b.generation)
                    {
                        best = Some((slot, md));
                    }
                }
                Err(OnDiskError::BadMagic) => {}
                Err(e) => tracing::warn!(slot, error = %e, "metadata slot invalid"),
            }
        }
        match best {
            Some((slot, md)) => {
                let next_slot = (slot + 1) % layout.meta_slots;
                Ok((
                    MetaRing {
                        layout,
                        next_slot,
                        generation: md.generation,
                    },
                    md,
                ))
            }
            None => match first_io_err {
                Some(e) => Err(OnDiskError::Io(e)),
                None => Err(OnDiskError::NoMetadata),
            },
        }
    }

    /// Stamp `md` with the next generation, write it to the next slot and advance.
    pub fn commit<B: BlockIo>(&mut self, io: &B, md: &mut Metadata) -> Result<u32> {
        let previous = md.generation;
        md.generation = self.generation + 1;
        let bytes = md.encode();
        if bytes.len() as u64 > self.layout.chunk_size {
            md.generation = previous;
            return Err(OnDiskError::InvalidLayout(format!(
                "metadata image ({} bytes) exceeds chunk size ({})",
                bytes.len(),
                self.layout.chunk_size
            )));
        }
        let slot = self.next_slot;
        if let Err(e) = io
            .write_at(&bytes, self.layout.meta_slot_offset(slot))
            .and_then(|_| io.sync())
        {
            md.generation = previous;
            return Err(e.into());
        }
        self.generation = md.generation;
        self.next_slot = (slot + 1) % self.layout.meta_slots;
        Ok(slot)
    }

    /// Zero the header sector of every slot so stale images from an earlier
    /// format can never win the generation race.
    pub fn invalidate_all<B: BlockIo>(io: &B, layout: &Layout) -> Result<()> {
        let zeros = vec![0u8; SECTOR_ALIGN];
        for slot in 0..layout.meta_slots {
            io.write_at(&zeros, layout.meta_slot_offset(slot))?;
        }
        io.sync()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::layout::MIB;
    use std::fs::File;

    fn temp_device(bytes: u64) -> File {
        let f = tempfile::tempfile().unwrap();
        f.set_len(bytes).unwrap();
        f
    }

    #[test]
    fn commit_rotates_and_load_picks_newest() {
        let layout = Layout::compute(128 * MIB, 4 * MIB, 3, 10).unwrap();
        let io = temp_device(128 * MIB);
        MetaRing::invalidate_all(&io, &layout).unwrap();
        assert!(matches!(
            MetaRing::load(&io, layout),
            Err(OnDiskError::NoMetadata)
        ));

        let mut ring = MetaRing::fresh(layout);
        let mut md = Metadata::initial(&layout);
        assert_eq!(ring.commit(&io, &mut md).unwrap(), 0);
        md.stats[9].write_count = 11;
        assert_eq!(ring.commit(&io, &mut md).unwrap(), 1);
        md.stats[9].write_count = 22;
        assert_eq!(ring.commit(&io, &mut md).unwrap(), 2);
        md.stats[9].write_count = 33;
        assert_eq!(ring.commit(&io, &mut md).unwrap(), 0);
        assert_eq!(md.generation, 4);

        let (ring2, loaded) = MetaRing::load(&io, layout).unwrap();
        assert_eq!(loaded, md);
        assert_eq!(ring2.next_slot(), 1);
    }

    #[test]
    fn committing_clones_still_advances_generation() {
        let layout = Layout::compute(128 * MIB, 4 * MIB, 3, 10).unwrap();
        let io = temp_device(128 * MIB);
        MetaRing::invalidate_all(&io, &layout).unwrap();
        let mut ring = MetaRing::fresh(layout);
        let base = Metadata::initial(&layout);
        for i in 1..=4u64 {
            let mut clone = base.clone();
            clone.stats[9].write_count = i;
            ring.commit(&io, &mut clone).unwrap();
            assert_eq!(clone.generation, i);
        }
        assert_eq!(ring.generation(), 4);
        let (_, loaded) = MetaRing::load(&io, layout).unwrap();
        assert_eq!(loaded.generation, 4);
        assert_eq!(loaded.stats[9].write_count, 4);
    }

    #[test]
    fn torn_newest_slot_falls_back() {
        let layout = Layout::compute(128 * MIB, 4 * MIB, 3, 10).unwrap();
        let io = temp_device(128 * MIB);
        MetaRing::invalidate_all(&io, &layout).unwrap();
        let mut ring = MetaRing::fresh(layout);
        let mut md = Metadata::initial(&layout);
        ring.commit(&io, &mut md).unwrap();
        let good = md.clone();
        md.stats[10].write_count = 5;
        let slot = ring.commit(&io, &mut md).unwrap();
        // Corrupt the newest image in the middle of its payload.
        let off = layout.meta_slot_offset(slot) + 100;
        io.write_at(&[0xAA; 8], off).unwrap();
        let (_, loaded) = MetaRing::load(&io, layout).unwrap();
        assert_eq!(loaded, good);
    }
}
