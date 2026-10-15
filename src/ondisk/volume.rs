//! A formatted device: superblock + layout + metadata ring, opened over a [`BlockIo`].

use super::layout::{Layout, DEFAULT_CHUNK_SIZE, DEFAULT_META_SLOTS, DEFAULT_SPARE_PCT};
use super::metadata::Metadata;
use super::ring::MetaRing;
use super::superblock::{ProbeReport, Superblock};
use super::{BlockIo, OnDiskError, Result};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct FormatOptions {
    pub chunk_size: u64,
    pub meta_slots: u32,
    pub spare_pct: u8,
    /// Use this UUID instead of generating one.
    pub uuid: Option<Uuid>,
    /// Overwrite an existing wlvl volume.
    pub force: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        FormatOptions {
            chunk_size: DEFAULT_CHUNK_SIZE,
            meta_slots: DEFAULT_META_SLOTS,
            spare_pct: DEFAULT_SPARE_PCT,
            uuid: None,
            force: false,
        }
    }
}

pub struct Volume<B: BlockIo> {
    io: B,
    superblock: Superblock,
    layout: Layout,
    ring: MetaRing,
    metadata: Metadata,
    probe: ProbeReport,
}

impl<B: BlockIo> Volume<B> {
    /// Write a fresh volume onto `io`. Destroys any data on the device.
    pub fn format(io: B, opts: &FormatOptions) -> Result<Volume<B>> {
        let device_len = io.len()?;
        let layout = Layout::compute(device_len, opts.chunk_size, opts.meta_slots, opts.spare_pct)?;
        let image_len = Metadata::image_len(&layout) as u64;
        if image_len > layout.chunk_size {
            return Err(OnDiskError::InvalidLayout(format!(
                "metadata image needs {image_len} bytes but a chunk is {}; increase --chunk-size",
                layout.chunk_size
            )));
        }
        if !opts.force {
            if let Ok((existing, _)) = Superblock::probe(&io) {
                return Err(OnDiskError::AlreadyFormatted(existing.uuid));
            }
        }
        let uuid = opts.uuid.unwrap_or_else(Uuid::new_v4);
        let superblock = Superblock::new(&layout, uuid);

        // Order matters: metadata first, superblocks last, so a crash mid-format
        // leaves a device without a valid superblock rather than a half volume.
        MetaRing::invalidate_all(&io, &layout)?;
        let mut ring = MetaRing::fresh(layout);
        let mut metadata = Metadata::initial(&layout);
        ring.commit(&io, &mut metadata)?;
        superblock.write_both(&io)?;

        Ok(Volume {
            io,
            superblock,
            layout,
            ring,
            metadata,
            probe: ProbeReport {
                primary_ok: true,
                mirror_ok: true,
            },
        })
    }

    /// Open an existing volume.
    pub fn open(io: B) -> Result<Volume<B>> {
        let (superblock, probe) = Superblock::probe(&io)?;
        let layout = superblock.layout()?;
        let (ring, metadata) = MetaRing::load(&io, layout)?;
        Ok(Volume {
            io,
            superblock,
            layout,
            ring,
            metadata,
            probe,
        })
    }

    /// Persist the in-memory metadata to the next ring slot.
    pub fn commit_metadata(&mut self) -> Result<u32> {
        self.ring.commit(&self.io, &mut self.metadata)
    }

    /// Re-write both superblock copies (repairs a damaged mirror/primary).
    pub fn repair_superblock(&mut self) -> Result<()> {
        self.superblock.write_both(&self.io)?;
        self.probe = ProbeReport {
            primary_ok: true,
            mirror_ok: true,
        };
        Ok(())
    }

    pub fn io(&self) -> &B {
        &self.io
    }

    pub fn uuid(&self) -> Uuid {
        self.superblock.uuid
    }

    pub fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut Metadata {
        &mut self.metadata
    }

    pub fn probe_report(&self) -> ProbeReport {
        self.probe
    }

    pub fn ring(&self) -> &MetaRing {
        &self.ring
    }

    /// Split into parts for callers that need to own the I/O handle and metadata separately.
    pub fn into_parts(self) -> (B, Superblock, Layout, MetaRing, Metadata) {
        (
            self.io,
            self.superblock,
            self.layout,
            self.ring,
            self.metadata,
        )
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
    fn format_then_open() {
        let io = temp_device(256 * MIB);
        let opts = FormatOptions {
            meta_slots: 4,
            ..Default::default()
        };
        let vol = Volume::format(io, &opts).unwrap();
        let uuid = vol.uuid();
        let (io, ..) = vol.into_parts();

        let vol = Volume::open(io).unwrap();
        assert_eq!(vol.uuid(), uuid);
        assert_eq!(vol.metadata().generation, 1);
        assert_eq!(vol.layout().meta_slots, 4);
        assert!(vol.probe_report().primary_ok && vol.probe_report().mirror_ok);
    }

    #[test]
    fn refuses_reformat_without_force() {
        let io = temp_device(256 * MIB);
        let vol = Volume::format(io, &FormatOptions::default()).unwrap();
        let uuid = vol.uuid();
        let (io, ..) = vol.into_parts();
        match Volume::format(io, &FormatOptions::default()) {
            Err(OnDiskError::AlreadyFormatted(u)) => assert_eq!(u, uuid),
            other => panic!(
                "expected AlreadyFormatted, got {:?}",
                other.map(|v| v.uuid())
            ),
        }
    }

    #[test]
    fn force_reformat_resets_generation() {
        let io = temp_device(256 * MIB);
        let mut vol = Volume::format(io, &FormatOptions::default()).unwrap();
        for _ in 0..5 {
            vol.commit_metadata().unwrap();
        }
        let (io, ..) = vol.into_parts();
        let vol = Volume::format(
            io,
            &FormatOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(vol.metadata().generation, 1);
    }

    #[test]
    fn opens_from_mirror_when_primary_damaged() {
        let io = temp_device(256 * MIB);
        let vol = Volume::format(io, &FormatOptions::default()).unwrap();
        let uuid = vol.uuid();
        let (io, ..) = vol.into_parts();
        io.write_at(&[0u8; 4096], 0).unwrap();
        let vol = Volume::open(io).unwrap();
        assert_eq!(vol.uuid(), uuid);
        assert!(!vol.probe_report().primary_ok && vol.probe_report().mirror_ok);
    }
}
