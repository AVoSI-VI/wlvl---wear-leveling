//! File-backed tests of the on-disk format through the public API.

use std::fs::File;
use wlvl::backing::ScanEntry;
use wlvl::ondisk::layout::MIB;
use wlvl::ondisk::{BlockIo, ChunkState, FormatOptions, Volume};

fn temp_device(bytes: u64) -> (tempfile::NamedTempFile, File) {
    let named = tempfile::NamedTempFile::new().unwrap();
    let f = named.reopen().unwrap();
    f.set_len(bytes).unwrap();
    (named, f)
}

#[test]
fn format_open_and_scan_entry_agree() {
    let (named, f) = temp_device(512 * MIB);
    let opts = FormatOptions {
        chunk_size: 8 * MIB,
        meta_slots: 4,
        spare_pct: 10,
        ..Default::default()
    };
    let vol = Volume::format(f, &opts).unwrap();
    let uuid = vol.uuid();
    let layout = *vol.layout();
    assert_eq!(layout.total_chunks, 64);
    assert_eq!(layout.pool_start(), 5);
    assert_eq!(layout.spare_chunks, 5);
    assert_eq!(layout.data_chunks, 53);

    let entry = ScanEntry::probe(named.path()).expect("file should probe as a wlvl volume");
    assert_eq!(entry.uuid, uuid);
    assert_eq!(entry.exported_bytes, layout.exported_bytes());
    assert!(entry.primary_ok && entry.mirror_ok);

    let reopened = Volume::open(named.reopen().unwrap()).unwrap();
    assert_eq!(reopened.uuid(), uuid);
    let md = reopened.metadata();
    assert_eq!(md.mapping.len() as u64, layout.data_chunks);
    assert_eq!(
        md.stats
            .iter()
            .filter(|s| s.state == ChunkState::Reserved)
            .count() as u64,
        2 + 4
    );
    assert_eq!(
        md.stats
            .iter()
            .filter(|s| s.state == ChunkState::Free)
            .count() as u64,
        layout.spare_chunks
    );
}

#[test]
fn metadata_survives_many_commits_and_crash_in_last() {
    let (named, f) = temp_device(256 * MIB);
    let mut vol = Volume::format(
        f,
        &FormatOptions {
            meta_slots: 3,
            ..Default::default()
        },
    )
    .unwrap();
    let layout = *vol.layout();
    for i in 0..10u64 {
        vol.metadata_mut().stats[layout.pool_start() as usize].write_count = i;
        vol.commit_metadata().unwrap();
    }
    let gen = vol.metadata().generation;
    let next = vol.ring().next_slot();
    // Simulate a torn write of the *next* commit: garbage in the slot it would use.
    let (io, ..) = vol.into_parts();
    let garbage = vec![0x5Au8; 4096];
    io.write_at(&garbage, layout.meta_slot_offset(next))
        .unwrap();

    let vol = Volume::open(named.reopen().unwrap()).unwrap();
    assert_eq!(vol.metadata().generation, gen);
    assert_eq!(
        vol.metadata().stats[layout.pool_start() as usize].write_count,
        9
    );
}

#[test]
fn unformatted_file_is_not_a_volume() {
    let (named, _f) = temp_device(64 * MIB);
    assert!(ScanEntry::probe(named.path()).is_none());
    assert!(Volume::open(named.reopen().unwrap()).is_err());
}
