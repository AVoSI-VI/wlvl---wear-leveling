//! Background persistence of the in-memory metadata to the slot ring.
//!
//! Commits happen on a timer, when the kernel issues FLUSH (rate limited),
//! when the wear-leveling worker asks for one, and once at shutdown.

use crate::mapper::ChunkMapper;
use crate::ondisk::{BlockIo, MetaRing, OnDiskError, Superblock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Default)]
struct SignalState {
    /// A hint that a commit would be nice (FLUSH); rate limited.
    hint: bool,
    /// Tickets handed out by `request_urgent`; always honoured promptly.
    requested: u64,
    /// Highest ticket whose commit has completed.
    acked: u64,
    /// Set when the persister can no longer commit (I/O errors).
    failed: bool,
}

/// Wakes the persister early and lets callers wait for a durable commit.
#[derive(Default)]
pub struct FlushSignal {
    state: Mutex<SignalState>,
    cv: Condvar,
    /// Generation of the newest durable metadata image.
    generation: AtomicU64,
}

/// What woke the persister.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    Timeout,
    Hint,
    Urgent,
}

impl FlushSignal {
    fn lock(&self) -> std::sync::MutexGuard<'_, SignalState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Rate-limited request (e.g. from FLUSH).
    pub fn notify(&self) {
        self.lock().hint = true;
        self.cv.notify_all();
    }

    /// Ask for a commit as soon as possible; returns a ticket for `wait_acked`.
    pub fn request_urgent(&self) -> u64 {
        let mut s = self.lock();
        s.requested += 1;
        let t = s.requested;
        self.cv.notify_all();
        t
    }

    /// Block until the commit for `ticket` is durable, the persister failed, or `d` elapses.
    pub fn wait_acked(&self, ticket: u64, d: Duration) -> Result<(), String> {
        let g = self.lock();
        let (g, _) = self
            .cv
            .wait_timeout_while(g, d, |s| s.acked < ticket && !s.failed)
            .unwrap_or_else(|e| e.into_inner());
        if g.acked >= ticket {
            Ok(())
        } else if g.failed {
            Err("metadata persister failed".into())
        } else {
            Err("timed out waiting for metadata commit".into())
        }
    }

    /// Persister side: wait for work or the timeout.
    pub fn wait_timeout(&self, d: Duration) -> Wake {
        let g = self.lock();
        let (mut g, _) = self
            .cv
            .wait_timeout_while(g, d, |s| !s.hint && s.requested == s.acked)
            .unwrap_or_else(|e| e.into_inner());
        let hint = std::mem::replace(&mut g.hint, false);
        if g.requested > g.acked {
            Wake::Urgent
        } else if hint {
            Wake::Hint
        } else {
            Wake::Timeout
        }
    }

    /// Persister side: the newest ticket issued so far. Read this *before*
    /// taking the metadata snapshot so the ack covers exactly what was durable.
    pub fn outstanding(&self) -> u64 {
        self.lock().requested
    }

    pub fn acked(&self) -> u64 {
        self.lock().acked
    }

    /// Persister side: a commit covering tickets up to `upto` completed.
    /// Clears a previous failure.
    pub fn ack(&self, upto: u64) {
        let mut s = self.lock();
        s.acked = s.acked.max(upto);
        s.failed = false;
        self.cv.notify_all();
    }

    pub fn mark_failed(&self) {
        self.lock().failed = true;
        self.cv.notify_all();
    }

    pub fn has_failed(&self) -> bool {
        self.lock().failed
    }

    pub fn set_generation(&self, g: u64) {
        self.generation.store(g, Ordering::Relaxed);
    }

    /// Generation of the newest durable metadata image.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// Owns the ring position and the buffered I/O handle used for metadata writes.
pub struct Persister<B: BlockIo> {
    io: B,
    ring: MetaRing,
    mapper: Arc<ChunkMapper>,
    /// Re-write both superblock copies on the next commit.
    repair_superblock: Option<Superblock>,
    last_commit: Instant,
}

#[derive(Debug, Clone)]
pub struct PersistOptions {
    /// Timer-driven commit interval.
    pub interval: Duration,
    /// Minimum spacing between FLUSH-triggered commits.
    pub min_flush_spacing: Duration,
}

impl Default for PersistOptions {
    fn default() -> Self {
        PersistOptions {
            interval: Duration::from_secs(30),
            min_flush_spacing: Duration::from_secs(5),
        }
    }
}

impl<B: BlockIo> Persister<B> {
    pub fn new(
        io: B,
        ring: MetaRing,
        mapper: Arc<ChunkMapper>,
        repair_superblock: Option<Superblock>,
    ) -> Self {
        Persister {
            io,
            ring,
            mapper,
            repair_superblock,
            last_commit: Instant::now(),
        }
    }

    /// Commit if anything changed. Returns `Ok(true)` when a commit was written.
    pub fn flush_if_dirty(&mut self) -> Result<bool, OnDiskError> {
        let (mut md, dirty) = self.mapper.take_snapshot();
        if !dirty && self.repair_superblock.is_none() {
            return Ok(false);
        }
        if let Some(sb) = self.repair_superblock.take() {
            if let Err(e) = sb.write_both(&self.io) {
                tracing::warn!(error = %e, "superblock repair failed");
                self.repair_superblock = Some(sb);
            } else {
                tracing::info!("superblock copies repaired");
            }
        }
        let slot = self.ring.commit(&self.io, &mut md)?;
        self.last_commit = Instant::now();
        tracing::debug!(generation = md.generation, slot, "metadata committed");
        Ok(true)
    }

    /// Force a commit regardless of the dirty flag (used at shutdown).
    pub fn flush(&mut self) -> Result<(), OnDiskError> {
        let (mut md, _) = self.mapper.take_snapshot();
        self.ring.commit(&self.io, &mut md)?;
        self.last_commit = Instant::now();
        Ok(())
    }

    pub fn ring(&self) -> &MetaRing {
        &self.ring
    }
}

/// Handle to a running persister thread.
pub struct PersistHandle {
    stop: Arc<AtomicBool>,
    signal: Arc<FlushSignal>,
    thread: Option<JoinHandle<()>>,
}

impl PersistHandle {
    /// Stop the thread; it performs a final commit before exiting.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.signal.notify();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PersistHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.signal.notify();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Spawn the persister loop. `signal` is shared with the I/O path and worker.
pub fn spawn<B: BlockIo + Send + 'static>(
    mut p: Persister<B>,
    signal: Arc<FlushSignal>,
    opts: PersistOptions,
) -> PersistHandle {
    signal.set_generation(p.ring.generation());
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let signal_t = signal.clone();
    let thread = std::thread::Builder::new()
        .name("wlvl-persist".into())
        .spawn(move || {
            loop {
                let wake = signal_t.wait_timeout(opts.interval);
                if stop_t.load(Ordering::SeqCst) {
                    break;
                }
                let ticket = signal_t.outstanding();
                // A ticket may have arrived after wait_timeout returned; treat it as urgent.
                let urgent = wake == Wake::Urgent || ticket > signal_t.acked();
                let result = if urgent {
                    p.flush().map(|_| true)
                } else if wake == Wake::Hint && p.last_commit.elapsed() < opts.min_flush_spacing {
                    continue;
                } else {
                    p.flush_if_dirty()
                };
                match result {
                    Ok(_) => {
                        signal_t.set_generation(p.ring.generation());
                        signal_t.ack(ticket);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "metadata commit failed");
                        signal_t.mark_failed();
                    }
                }
            }
            let ticket = signal_t.outstanding();
            match p.flush() {
                Ok(()) => {
                    tracing::info!("final metadata commit written");
                    signal_t.set_generation(p.ring.generation());
                    signal_t.ack(ticket);
                }
                Err(e) => {
                    tracing::error!(error = %e, "final metadata commit failed");
                    signal_t.mark_failed();
                }
            }
        })
        .expect("spawning persist thread");
    PersistHandle {
        stop,
        signal,
        thread: Some(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapper::{IoOutcome, Mapper};
    use crate::ondisk::layout::MIB;
    use crate::ondisk::{FormatOptions, Volume};

    fn setup() -> (tempfile::NamedTempFile, Arc<ChunkMapper>, MetaRing) {
        let named = tempfile::NamedTempFile::new().unwrap();
        let f = named.reopen().unwrap();
        f.set_len(128 * MIB).unwrap();
        let vol = Volume::format(
            f,
            &FormatOptions {
                meta_slots: 3,
                ..Default::default()
            },
        )
        .unwrap();
        let (_io, _sb, layout, ring, md) = vol.into_parts();
        (named, Arc::new(ChunkMapper::new(layout, md)), ring)
    }

    #[test]
    fn flush_if_dirty_only_commits_changes() {
        let (named, mapper, ring) = setup();
        let mut p = Persister::new(named.reopen().unwrap(), ring, mapper.clone(), None);
        assert!(!p.flush_if_dirty().unwrap());
        let pool = Mapper::layout(&*mapper).pool_start();
        mapper.end_write(
            0,
            0,
            512,
            IoOutcome {
                latency: Duration::from_micros(10),
                ok: true,
            },
        );
        assert!(p.flush_if_dirty().unwrap());
        assert!(!p.flush_if_dirty().unwrap());
        let vol = Volume::open(named.reopen().unwrap()).unwrap();
        assert_eq!(vol.metadata().generation, 2);
        assert_eq!(vol.metadata().stats[pool as usize].write_count, 1);
    }

    #[test]
    fn thread_commits_on_signal_and_shutdown() {
        let (named, mapper, ring) = setup();
        let p = Persister::new(named.reopen().unwrap(), ring, mapper.clone(), None);
        let signal = Arc::new(FlushSignal::default());
        let opts = PersistOptions {
            interval: Duration::from_secs(60),
            min_flush_spacing: Duration::ZERO,
        };
        let handle = spawn(p, signal.clone(), opts);

        mapper.end_write(
            1,
            0,
            512,
            IoOutcome {
                latency: Duration::from_micros(10),
                ok: true,
            },
        );
        signal.notify();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let vol = Volume::open(named.reopen().unwrap()).unwrap();
            if vol.metadata().generation >= 2 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "persister did not commit on signal"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // Urgent request: always commits and is acknowledged.
        let ticket = signal.request_urgent();
        signal.wait_acked(ticket, Duration::from_secs(5)).unwrap();
        assert_eq!(
            Volume::open(named.reopen().unwrap())
                .unwrap()
                .metadata()
                .generation,
            3
        );

        handle.shutdown();
        let vol = Volume::open(named.reopen().unwrap()).unwrap();
        assert_eq!(vol.metadata().generation, 4);
    }

    #[test]
    fn signal_wake_reasons() {
        let s = FlushSignal::default();
        assert_eq!(s.wait_timeout(Duration::from_millis(1)), Wake::Timeout);
        s.notify();
        assert_eq!(s.wait_timeout(Duration::from_millis(1)), Wake::Hint);
        let t = s.request_urgent();
        assert_eq!(s.wait_timeout(Duration::from_millis(1)), Wake::Urgent);
        assert!(s.wait_acked(t, Duration::from_millis(1)).is_err());
        assert_eq!(s.outstanding(), t);
        s.ack(t);
        assert_eq!(s.acked(), t);
        s.wait_acked(t, Duration::from_millis(1)).unwrap();
        s.mark_failed();
        let t2 = s.request_urgent();
        assert!(s
            .wait_acked(t2, Duration::from_millis(50))
            .unwrap_err()
            .contains("failed"));
    }

    #[test]
    fn repairs_superblock_on_commit() {
        let (named, mapper, ring) = setup();
        let io = named.reopen().unwrap();
        let (sb, _) = Superblock::probe(&io).unwrap();
        io.write_at(&[0u8; 4096], 0).unwrap();
        assert!(!Superblock::probe(&io).unwrap().1.primary_ok);
        let mut p = Persister::new(io, ring, mapper, Some(sb));
        assert!(p.flush_if_dirty().unwrap());
        assert!(
            Superblock::probe(&named.reopen().unwrap())
                .unwrap()
                .1
                .primary_ok
        );
    }
}
