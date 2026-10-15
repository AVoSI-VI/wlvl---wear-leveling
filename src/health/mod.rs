//! Health assessment: inferred counters plus best-effort hardware sources,
//! combined into the `status` report.

pub mod inferred;
pub mod mmc_sysfs;
pub mod smart;

pub use inferred::{assess, HealthParams, HealthReport};
pub use mmc_sysfs::MmcReport;
pub use smart::SmartReport;

use crate::mapper::{ChunkMapper, MigrationInfo};
use crate::ondisk::{BlockIo, Layout, Volume};
use crate::wear::WearStats;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareReport {
    pub smart: SmartReport,
    pub mmc: MmcReport,
}

/// Query every hardware source for `dev`. Each reports its own availability.
pub fn hardware(dev: &Path) -> HardwareReport {
    HardwareReport {
        smart: smart::query(dev),
        mmc: mmc_sysfs::query(dev),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub chunk_size: u64,
    pub total_chunks: u64,
    pub data_chunks: u64,
    pub spare_chunks: u64,
    pub meta_slots: u32,
    pub exported_bytes: u64,
}

impl From<&Layout> for Geometry {
    fn from(l: &Layout) -> Self {
        Geometry {
            chunk_size: l.chunk_size,
            total_chunks: l.total_chunks,
            data_chunks: l.data_chunks,
            spare_chunks: l.spare_chunks,
            meta_slots: l.meta_slots,
            exported_bytes: l.exported_bytes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub uuid: Uuid,
    pub device: PathBuf,
    /// `/dev/ublkbN` when attached.
    pub attached: Option<String>,
    /// `true` when produced by the running daemon (counters are current).
    pub live: bool,
    pub generation: u64,
    pub geometry: Geometry,
    pub health: HealthReport,
    pub migration: Option<MigrationInfo>,
    pub wear: Option<WearStats>,
    pub hardware: Option<HardwareReport>,
}

impl StatusReport {
    /// Report from an opened (not attached) volume.
    pub fn offline<B: BlockIo>(
        device: &Path,
        vol: &Volume<B>,
        params: &HealthParams,
        with_hardware: bool,
    ) -> StatusReport {
        let layout = vol.layout();
        StatusReport {
            uuid: vol.uuid(),
            device: device.to_path_buf(),
            attached: None,
            live: false,
            generation: vol.metadata().generation,
            geometry: layout.into(),
            health: assess(layout, vol.metadata(), params),
            migration: None,
            wear: None,
            hardware: with_hardware.then(|| hardware(device)),
        }
    }

    /// Report from the live mapper inside the daemon.
    #[allow(clippy::too_many_arguments)]
    pub fn live(
        uuid: Uuid,
        device: &Path,
        attached: Option<String>,
        mapper: &ChunkMapper,
        generation: u64,
        wear: Option<WearStats>,
        params: &HealthParams,
        with_hardware: bool,
    ) -> StatusReport {
        let layout = *crate::mapper::Mapper::layout(mapper);
        let health = mapper.with_metadata(|md| assess(&layout, md, params));
        StatusReport {
            uuid,
            device: device.to_path_buf(),
            attached,
            live: true,
            generation,
            geometry: (&layout).into(),
            health,
            migration: mapper.current_migration(),
            wear,
            hardware: with_hardware.then(|| hardware(device)),
        }
    }
}
