//! File-backed end-to-end tests of the wear-leveling worker: data must survive
//! migration, including migration under concurrent writes, and the on-disk
//! image must stay consistent and reloadable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wlvl::iopath;
use wlvl::mapper::{ChunkMapper, Mapper};
use wlvl::ondisk::layout::MIB;
use wlvl::ondisk::{ChunkState, FormatOptions, Volume};
use wlvl::persist::{self, FlushSignal, PersistOptions, Persister};
use wlvl::wear::{self, Plan, PolicyParams, Reason, WearOptions, WearStats};

struct Rig {
    named: tempfile::NamedTempFile,
    mapper: Arc<ChunkMapper>,
    signal: Arc<FlushSignal>,
    persist: Option<persist::PersistHandle>,
}

fn rig() -> Rig {
    let named = tempfile::NamedTempFile::new().unwrap();
    let f = named.reopen().unwrap();
    f.set_len(128 * MIB).unwrap();
    let opts = FormatOptions {
        chunk_size: 4 * MIB,
        meta_slots: 3,
        spare_pct: 10,
        ..Default::default()
    };
    let vol = Volume::format(f, &opts).unwrap();
    let (_io, _sb, layout, ring, md) = vol.into_parts();
    let mapper = Arc::new(ChunkMapper::new(layout, md));
    let signal = Arc::new(FlushSignal::default());
    let p = Persister::new(named.reopen().unwrap(), ring, mapper.clone(), None);
    let popts = PersistOptions {
        interval: Duration::from_secs(60),
        min_flush_spacing: Duration::ZERO,
    };
    let persist = Some(persist::spawn(p, signal.clone(), popts));
    Rig {
        named,
        mapper,
        signal,
        persist,
    }
}

fn pattern(seed: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64).wrapping_mul(31).wrapping_add(seed * 7) % 253) as u8)
        .collect()
}

fn fast_opts() -> WearOptions {
    WearOptions {
        policy: PolicyParams {
            wear_delta_threshold: 10,
            ..Default::default()
        },
        migrate: wear::MigrateOptions {
            piece_bytes: 256 * 1024,
            poll_interval: Duration::from_millis(5),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn migration_preserves_data_and_persists_mapping() {
    let r = rig();
    let io = r.named.reopen().unwrap();
    let layout = *r.mapper.layout();
    let cs = layout.chunk_size;

    // Fill logical chunks 0..3 with distinct patterns.
    for l in 0..3u64 {
        iopath::write(&*r.mapper, &io, l * cs, &pattern(l, cs as usize)).unwrap();
    }
    // Make chunk 1 look hot and worn so the policy picks it.
    for _ in 0..40 {
        iopath::write(&*r.mapper, &io, cs + 4096, &pattern(1, 4096)[..]).unwrap();
    }
    let before = r.mapper.resolve_read(1);
    let stop = AtomicBool::new(false);
    let stats = Mutex::new(WearStats::default());
    let ran = wear::run_once(&r.mapper, &io, &r.signal, &fast_opts(), &stop, &stats).unwrap();
    assert!(ran, "policy should have produced a plan");
    let report = stats.lock().unwrap().last_report.unwrap();
    assert_eq!(report.plan.reason, Reason::Dynamic);
    assert_eq!(report.plan.lchunk, 1);
    assert_eq!(report.freed, before);
    assert_ne!(r.mapper.resolve_read(1), before);

    // Data intact through the new mapping.
    for l in 0..3u64 {
        let mut back = vec![0u8; cs as usize];
        iopath::read(&*r.mapper, &io, l * cs, &mut back).unwrap();
        let mut expect = pattern(l, cs as usize);
        if l == 1 {
            expect[4096..8192].copy_from_slice(&pattern(1, 4096));
        }
        assert!(back == expect, "logical chunk {l} corrupted");
    }

    // On-disk image already carries the new mapping (commit happened before completion).
    let vol = Volume::open(r.named.reopen().unwrap()).unwrap();
    assert_eq!(vol.metadata().physical_of(1), r.mapper.resolve_read(1));
    assert_eq!(
        vol.metadata().stats[before as usize].state,
        ChunkState::Free
    );
    vol.metadata().validate(&layout).unwrap();
    assert!(r.mapper.current_migration().is_none());
}

#[test]
fn migration_under_concurrent_writes_keeps_latest_data() {
    let r = rig();
    let io = r.named.reopen().unwrap();
    let layout = *r.mapper.layout();
    let cs = layout.chunk_size;
    iopath::write(&*r.mapper, &io, 0, &pattern(9, cs as usize)).unwrap();

    // Writer thread: keeps rewriting 4 KiB blocks of logical chunk 0 with increasing versions.
    let stop_writer = Arc::new(AtomicBool::new(false));
    let latest: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(vec![0; (cs / 4096) as usize]));
    let writer = {
        let (mapper, stop_w, latest, wio) = (
            r.mapper.clone(),
            stop_writer.clone(),
            latest.clone(),
            r.named.reopen().unwrap(),
        );
        std::thread::spawn(move || {
            let mut v = 1u32;
            let mut i = 0usize;
            while !stop_w.load(Ordering::SeqCst) {
                let blk = (i * 7919) % (cs as usize / 4096);
                let data = pattern(v as u64, 4096);
                iopath::write(&*mapper, &wio, (blk * 4096) as u64, &data).unwrap();
                latest.lock().unwrap()[blk] = v;
                v += 1;
                i += 1;
            }
        })
    };

    let dst = r
        .mapper
        .with_metadata(|md| md.free_chunks().next().unwrap());
    let plan = Plan {
        lchunk: 0,
        src: r.mapper.resolve_read(0),
        dst,
        reason: Reason::Dynamic,
    };
    let stop = AtomicBool::new(false);
    let report = wear::migrate::migrate(
        &r.mapper,
        &io,
        &r.signal,
        &plan,
        &fast_opts().migrate,
        &stop,
    )
    .unwrap();
    assert_eq!(r.mapper.resolve_read(0), dst);
    stop_writer.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    assert!(report.passes >= 1);

    // Every block must hold the newest version the writer recorded (or the base pattern).
    let versions = latest.lock().unwrap().clone();
    let mut back = vec![0u8; cs as usize];
    iopath::read(&*r.mapper, &io, 0, &mut back).unwrap();
    let base = pattern(9, cs as usize);
    for (blk, &v) in versions.iter().enumerate() {
        let got = &back[blk * 4096..(blk + 1) * 4096];
        let expect = if v == 0 {
            base[blk * 4096..(blk + 1) * 4096].to_vec()
        } else {
            pattern(v as u64, 4096)
        };
        assert!(
            got == &expect[..],
            "block {blk} version {v} mismatch after migration"
        );
    }
    // Reload from disk: mapping durable and consistent.
    let vol = Volume::open(r.named.reopen().unwrap()).unwrap();
    assert_eq!(vol.metadata().physical_of(0), dst);
    vol.metadata().validate(&layout).unwrap();
}

#[test]
fn failing_chunk_is_evacuated_and_retired() {
    let mut r = rig();
    let io = r.named.reopen().unwrap();
    let layout = *r.mapper.layout();
    let cs = layout.chunk_size;
    iopath::write(&*r.mapper, &io, 2 * cs, &pattern(5, cs as usize)).unwrap();
    let bad = r.mapper.resolve_read(2);
    for _ in 0..3 {
        r.mapper.end_write(
            2,
            0,
            512,
            wlvl::mapper::IoOutcome {
                latency: Duration::from_micros(5),
                ok: false,
            },
        );
    }
    let stop = AtomicBool::new(false);
    let stats = Mutex::new(WearStats::default());
    assert!(wear::run_once(&r.mapper, &io, &r.signal, &fast_opts(), &stop, &stats).unwrap());
    let s = stats.lock().unwrap().clone();
    assert_eq!(s.last_report.unwrap().plan.reason, Reason::Retire);
    assert_eq!(s.chunks_retired, 1);
    r.mapper
        .with_metadata(|md| assert_eq!(md.stats[bad as usize].state, ChunkState::Retired));
    let mut back = vec![0u8; cs as usize];
    iopath::read(&*r.mapper, &io, 2 * cs, &mut back).unwrap();
    assert!(back == pattern(5, cs as usize));

    // Shutdown flushes the retirement; it must survive a reload.
    r.persist.take().unwrap().shutdown();
    let vol = Volume::open(r.named.reopen().unwrap()).unwrap();
    assert_eq!(
        vol.metadata().stats[bad as usize].state,
        ChunkState::Retired
    );
    vol.metadata().validate(&layout).unwrap();
}
