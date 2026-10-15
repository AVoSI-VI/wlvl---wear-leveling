//! Logical-to-physical chunk mapping shared between the I/O path and the
//! wear-leveling worker.
//!
//! Migration never blocks I/O. It runs in two phases:
//!
//! 1. `Copying`: reads go to the old chunk, writes go to old *and* new; writes
//!    that complete are recorded as dirty ranges for the worker to re-copy.
//! 2. `Committing`: the live mapping points at the new chunk but writes still
//!    mirror to the old one until the worker has made the new mapping durable.
//!    Only then is the old chunk released. A crash at any point leaves a
//!    consistent on-disk image whose mapped chunk holds complete data.

use crate::ondisk::{ChunkState, ChunkStats, Layout, Metadata};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Where a request for one logical chunk must go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// Physical chunk currently holding the authoritative data.
    pub primary: u64,
    /// Second chunk that must receive the same write (migration in progress).
    pub mirror: Option<u64>,
}

/// Outcome of one backing I/O, reported back to the mapper.
#[derive(Debug, Clone, Copy)]
pub struct IoOutcome {
    pub latency: Duration,
    pub ok: bool,
}

/// Minimal interface the I/O path needs. `ChunkMapper` is the only implementation.
pub trait Mapper: Send + Sync {
    fn layout(&self) -> &Layout;
    fn resolve_read(&self, lchunk: u64) -> u64;
    /// Resolve a write and register it as in flight.
    fn begin_write(&self, lchunk: u64) -> Resolved;
    /// Report a finished write (`off`/`len` are relative to the chunk).
    fn end_write(&self, lchunk: u64, off: u64, len: u64, outcome: IoOutcome);
    /// Report a finished read on `pchunk`.
    fn end_read(&self, pchunk: u64, outcome: IoOutcome);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Copying,
    Committing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MigrationInfo {
    pub lchunk: u64,
    pub src: u64,
    pub dst: u64,
    pub phase: Phase,
}

#[derive(Debug, Clone)]
struct Migration {
    info: MigrationInfo,
    inflight: u32,
    /// (offset, len) ranges written since the worker last drained them.
    dirty: Vec<(u64, u64)>,
    /// While set, new writes to this logical chunk wait (final switch of a hot chunk).
    barrier: bool,
}

/// Upper bound on how long a write waits at a migration barrier.
pub const BARRIER_MAX_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Inner {
    metadata: Metadata,
    /// Per-logical-chunk write heat (decayed by the worker).
    hotness: Vec<u32>,
    migration: Option<Migration>,
    dirty_since_snapshot: bool,
}

pub struct ChunkMapper {
    layout: Layout,
    inner: Mutex<Inner>,
    /// Signalled when a migration barrier is lowered.
    barrier_cv: Condvar,
    /// Monotonic count of completed foreground writes (for load estimation).
    writes_total: AtomicU64,
}

impl ChunkMapper {
    pub fn new(layout: Layout, metadata: Metadata) -> ChunkMapper {
        let hotness = vec![0; layout.data_chunks as usize];
        ChunkMapper {
            layout,
            inner: Mutex::new(Inner {
                metadata,
                hotness,
                migration: None,
                dirty_since_snapshot: false,
            }),
            barrier_cv: Condvar::new(),
            writes_total: AtomicU64::new(0),
        }
    }

    /// Hold new writes to the migrating chunk until `finish_copy` succeeds or
    /// the migration is aborted.
    pub fn raise_barrier(&self) -> Result<(), String> {
        let mut g = self.lock();
        let m = g.migration.as_mut().ok_or("no migration in progress")?;
        m.barrier = true;
        Ok(())
    }

    fn lower_barrier(g: &mut Inner, cv: &Condvar) {
        if let Some(m) = g.migration.as_mut() {
            m.barrier = false;
        }
        cv.notify_all();
    }

    pub fn writes_total(&self) -> u64 {
        self.writes_total.load(Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copy of the current metadata for persisting. Clears the dirty flag.
    pub fn take_snapshot(&self) -> (Metadata, bool) {
        let mut g = self.lock();
        let dirty = std::mem::replace(&mut g.dirty_since_snapshot, false);
        (g.metadata.clone(), dirty)
    }

    /// Run `f` against the metadata (for status/health readers).
    pub fn with_metadata<R>(&self, f: impl FnOnce(&Metadata) -> R) -> R {
        f(&self.lock().metadata)
    }

    /// Current per-logical-chunk heat values.
    pub fn hotness_snapshot(&self) -> Vec<u32> {
        self.lock().hotness.clone()
    }

    /// Halve every heat value (called periodically by the worker).
    pub fn decay_hotness(&self) {
        for h in self.lock().hotness.iter_mut() {
            *h >>= 1;
        }
    }

    pub fn current_migration(&self) -> Option<MigrationInfo> {
        self.lock().migration.as_ref().map(|m| m.info)
    }

    /// Start moving `lchunk` onto `dst`, which must be `Free`. `dst` stays
    /// `Free` on disk until the switch so an interrupted copy is simply forgotten.
    pub fn begin_migration(&self, lchunk: u64, dst: u64) -> Result<MigrationInfo, String> {
        let mut g = self.lock();
        if g.migration.is_some() {
            return Err("a migration is already in progress".into());
        }
        if lchunk >= self.layout.data_chunks {
            return Err(format!("logical chunk {lchunk} out of range"));
        }
        if dst >= self.layout.total_chunks
            || g.metadata.stats[dst as usize].state != ChunkState::Free
        {
            return Err(format!("physical chunk {dst} is not free"));
        }
        let src = g.metadata.physical_of(lchunk);
        let info = MigrationInfo {
            lchunk,
            src,
            dst,
            phase: Phase::Copying,
        };
        g.migration = Some(Migration {
            info,
            inflight: 0,
            dirty: Vec::new(),
            barrier: false,
        });
        Ok(info)
    }

    /// Take the ranges dirtied since the last drain, plus the number of writes
    /// still in flight for the migrating chunk.
    pub fn drain_dirty(&self) -> Result<(Vec<(u64, u64)>, u32), String> {
        let mut g = self.lock();
        let m = g.migration.as_mut().ok_or("no migration in progress")?;
        Ok((std::mem::take(&mut m.dirty), m.inflight))
    }

    /// Switch the live mapping to the destination if the chunk is quiescent.
    /// Returns `Ok(Err(dirty))` when writes intervened; the caller re-copies.
    pub fn finish_copy(&self) -> Result<Result<(), Vec<(u64, u64)>>, String> {
        let mut g = self.lock();
        let m = g.migration.as_mut().ok_or("no migration in progress")?;
        if m.info.phase != Phase::Copying {
            return Err("migration is not in the copying phase".into());
        }
        if m.inflight != 0 || !m.dirty.is_empty() {
            return Ok(Err(std::mem::take(&mut m.dirty)));
        }
        m.info.phase = Phase::Committing;
        let MigrationInfo {
            lchunk, src, dst, ..
        } = m.info;
        g.metadata.mapping[lchunk as usize] = dst as u32;
        g.metadata.stats[dst as usize].state = ChunkState::InUse;
        g.metadata.stats[src as usize].state = ChunkState::Free;
        g.dirty_since_snapshot = true;
        Self::lower_barrier(&mut g, &self.barrier_cv);
        Ok(Ok(()))
    }

    /// The new mapping is durable: stop mirroring and release the old chunk.
    pub fn complete_migration(&self) -> Result<u64, String> {
        let mut g = self.lock();
        match g.migration.take() {
            Some(m) if m.info.phase == Phase::Committing => Ok(m.info.src),
            Some(m) => {
                g.migration = Some(m);
                Err("migration has not been switched yet".into())
            }
            None => Err("no migration in progress".into()),
        }
    }

    /// Abandon a migration. In `Committing` the mapping is restored to the old
    /// chunk, which is still complete thanks to mirroring.
    pub fn abort_migration(&self, retire_dst: bool) {
        let mut g = self.lock();
        let Some(m) = g.migration.take() else { return };
        let MigrationInfo {
            lchunk,
            src,
            dst,
            phase,
        } = m.info;
        if phase == Phase::Committing {
            g.metadata.mapping[lchunk as usize] = src as u32;
            g.metadata.stats[src as usize].state = ChunkState::InUse;
        }
        g.metadata.stats[dst as usize].state = if retire_dst {
            ChunkState::Retired
        } else {
            ChunkState::Free
        };
        g.dirty_since_snapshot = true;
        self.barrier_cv.notify_all();
    }

    /// Mark a `Free` physical chunk as retired.
    pub fn retire_free_chunk(&self, pchunk: u64) -> bool {
        let mut g = self.lock();
        if g.migration.as_ref().is_some_and(|m| m.info.dst == pchunk) {
            return false;
        }
        let s = &mut g.metadata.stats[pchunk as usize];
        if s.state != ChunkState::Free {
            return false;
        }
        s.state = ChunkState::Retired;
        g.dirty_since_snapshot = true;
        true
    }

    /// Account a background (worker) write to `pchunk`.
    pub fn record_background_write(&self, pchunk: u64, outcome: IoOutcome) {
        let mut g = self.lock();
        Self::account(&mut g.metadata.stats[pchunk as usize], outcome, true);
        g.dirty_since_snapshot = true;
    }

    /// Account a verification failure on `pchunk`.
    pub fn record_verify_failure(&self, pchunk: u64) {
        let mut g = self.lock();
        let s = &mut g.metadata.stats[pchunk as usize];
        s.verify_failures = s.verify_failures.saturating_add(1);
        g.dirty_since_snapshot = true;
    }

    fn account(stats: &mut ChunkStats, outcome: IoOutcome, is_write: bool) {
        if is_write {
            stats.write_count = stats.write_count.saturating_add(1);
        }
        if !outcome.ok {
            stats.io_errors = stats.io_errors.saturating_add(1);
        }
        let us = outcome.latency.as_micros().min(u32::MAX as u128) as u32;
        // EWMA with alpha = 1/8.
        stats.latency_ewma_us = if stats.latency_ewma_us == 0 {
            us
        } else {
            stats.latency_ewma_us - stats.latency_ewma_us / 8 + us / 8
        };
    }
}

impl Mapper for ChunkMapper {
    fn layout(&self) -> &Layout {
        &self.layout
    }

    fn resolve_read(&self, lchunk: u64) -> u64 {
        self.lock().metadata.physical_of(lchunk)
    }

    fn begin_write(&self, lchunk: u64) -> Resolved {
        let mut g = self.lock();
        if g.migration
            .as_ref()
            .is_some_and(|m| m.barrier && m.info.lchunk == lchunk)
        {
            let (guard, _) = self
                .barrier_cv
                .wait_timeout_while(g, BARRIER_MAX_WAIT, |i| {
                    i.migration
                        .as_ref()
                        .is_some_and(|m| m.barrier && m.info.lchunk == lchunk)
                })
                .unwrap_or_else(|e| e.into_inner());
            g = guard;
        }
        let mapped = g.metadata.physical_of(lchunk);
        match g.migration.as_mut() {
            Some(m) if m.info.lchunk == lchunk => {
                m.inflight += 1;
                match m.info.phase {
                    Phase::Copying => Resolved {
                        primary: m.info.src,
                        mirror: Some(m.info.dst),
                    },
                    Phase::Committing => Resolved {
                        primary: m.info.dst,
                        mirror: Some(m.info.src),
                    },
                }
            }
            _ => Resolved {
                primary: mapped,
                mirror: None,
            },
        }
    }

    fn end_write(&self, lchunk: u64, off: u64, len: u64, outcome: IoOutcome) {
        self.writes_total.fetch_add(1, Ordering::Relaxed);
        let mut g = self.lock();
        let mapped = g.metadata.physical_of(lchunk);
        Self::account(&mut g.metadata.stats[mapped as usize], outcome, true);
        g.hotness[lchunk as usize] = g.hotness[lchunk as usize].saturating_add(1);
        g.dirty_since_snapshot = true;
        if let Some(m) = g.migration.as_mut() {
            if m.info.lchunk == lchunk {
                m.inflight = m.inflight.saturating_sub(1);
                m.dirty.push((off, len));
                let other = if m.info.phase == Phase::Copying {
                    m.info.dst
                } else {
                    m.info.src
                };
                Self::account(&mut g.metadata.stats[other as usize], outcome, true);
            }
        }
    }

    fn end_read(&self, pchunk: u64, outcome: IoOutcome) {
        let mut g = self.lock();
        Self::account(&mut g.metadata.stats[pchunk as usize], outcome, false);
        if !outcome.ok {
            g.dirty_since_snapshot = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::layout::MIB;

    fn mapper() -> ChunkMapper {
        let layout = Layout::compute(256 * MIB, 4 * MIB, 4, 10).unwrap();
        let md = Metadata::initial(&layout);
        ChunkMapper::new(layout, md)
    }

    fn ok(us: u64) -> IoOutcome {
        IoOutcome {
            latency: Duration::from_micros(us),
            ok: true,
        }
    }

    #[test]
    fn identity_mapping_and_accounting() {
        let m = mapper();
        let pool = m.layout().pool_start();
        assert_eq!(m.resolve_read(0), pool);
        assert_eq!(
            m.begin_write(3),
            Resolved {
                primary: pool + 3,
                mirror: None
            }
        );
        m.end_write(3, 0, 4096, ok(800));
        m.end_write(
            3,
            0,
            4096,
            IoOutcome {
                latency: Duration::from_micros(1600),
                ok: false,
            },
        );
        let (md, dirty) = m.take_snapshot();
        assert!(dirty);
        let s = md.stats[(pool + 3) as usize];
        assert_eq!(
            (s.write_count, s.io_errors, s.latency_ewma_us),
            (2, 1, 800 - 100 + 200)
        );
        assert_eq!(m.hotness_snapshot()[3], 2);
        m.decay_hotness();
        assert_eq!(m.hotness_snapshot()[3], 1);
        assert!(!m.take_snapshot().1);
    }

    #[test]
    fn two_phase_migration() {
        let m = mapper();
        let pool = m.layout().pool_start();
        let dst = m.with_metadata(|md| md.free_chunks().next().unwrap());
        let info = m.begin_migration(2, dst).unwrap();
        assert_eq!(info.phase, Phase::Copying);
        assert!(m.begin_migration(3, dst).is_err());
        // dst stays Free on disk while copying.
        m.with_metadata(|md| assert_eq!(md.stats[dst as usize].state, ChunkState::Free));

        // Write during copy: mirrored to dst, dirty on completion.
        assert_eq!(
            m.begin_write(2),
            Resolved {
                primary: pool + 2,
                mirror: Some(dst)
            }
        );
        assert_eq!(m.begin_write(5).mirror, None);
        assert_eq!(m.finish_copy().unwrap(), Err(vec![]));
        m.end_write(2, 8192, 4096, ok(500));
        assert_eq!(m.drain_dirty().unwrap(), (vec![(8192, 4096)], 0));
        assert_eq!(m.finish_copy().unwrap(), Ok(()));

        // Committing: reads and primary writes go to dst, mirror to src.
        assert_eq!(m.resolve_read(2), dst);
        assert_eq!(
            m.begin_write(2),
            Resolved {
                primary: dst,
                mirror: Some(pool + 2)
            }
        );
        m.end_write(2, 0, 512, ok(10));
        m.with_metadata(|md| {
            assert_eq!(md.stats[(pool + 2) as usize].state, ChunkState::Free);
            assert_eq!(md.stats[dst as usize].state, ChunkState::InUse);
            md.validate(m.layout()).unwrap();
        });
        assert_eq!(m.complete_migration().unwrap(), pool + 2);
        assert_eq!(
            m.begin_write(2),
            Resolved {
                primary: dst,
                mirror: None
            }
        );
        assert!(m.current_migration().is_none());
    }

    #[test]
    fn barrier_holds_writes_until_switch() {
        let m = std::sync::Arc::new(mapper());
        let dst = m.with_metadata(|md| md.free_chunks().next().unwrap());
        m.begin_migration(4, dst).unwrap();
        m.raise_barrier().unwrap();
        // Other chunks are unaffected.
        assert_eq!(m.begin_write(5).mirror, None);
        let m2 = m.clone();
        let t = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let r = m2.begin_write(4);
            (r, started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(m.finish_copy().unwrap(), Ok(()));
        let (r, waited) = t.join().unwrap();
        assert!(
            waited >= Duration::from_millis(80),
            "write should have waited at the barrier"
        );
        assert_eq!(
            r.primary, dst,
            "write released after the switch goes to the new chunk"
        );
    }

    #[test]
    fn abort_paths_and_retire() {
        let m = mapper();
        let pool = m.layout().pool_start();
        let dst = m.with_metadata(|md| md.free_chunks().next().unwrap());
        m.begin_migration(0, dst).unwrap();
        m.finish_copy().unwrap().unwrap();
        m.abort_migration(true);
        assert_eq!(m.resolve_read(0), pool);
        m.with_metadata(|md| {
            assert_eq!(md.stats[dst as usize].state, ChunkState::Retired);
            assert_eq!(md.stats[pool as usize].state, ChunkState::InUse);
            md.validate(m.layout()).unwrap();
        });
        let other = m.with_metadata(|md| md.free_chunks().next().unwrap());
        m.begin_migration(1, other).unwrap();
        assert!(
            !m.retire_free_chunk(other),
            "cannot retire a migration destination"
        );
        m.abort_migration(false);
        assert!(m.retire_free_chunk(other));
        assert!(!m.retire_free_chunk(pool));
    }
}
