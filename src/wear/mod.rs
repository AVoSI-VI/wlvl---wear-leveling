//! Background wear-leveling worker.
//!
//! Periodically consults [`policy::choose`] and executes the resulting plan
//! with [`migrate::migrate`]. One migration at a time; postponed while the
//! device is busy.

pub mod migrate;
pub mod policy;

pub use migrate::{MigrateError, MigrateOptions, MigrateReport};
pub use policy::{Plan, PolicyParams, Reason, WearSummary};

use crate::mapper::ChunkMapper;
use crate::ondisk::BlockIo;
use crate::persist::FlushSignal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct WearOptions {
    /// How often the policy is consulted.
    pub check_interval: Duration,
    /// Minimum spacing between two migrations.
    pub rotate_interval: Duration,
    /// Heat values are halved this often.
    pub hotness_decay_interval: Duration,
    /// Skip migrations while foreground writes exceed this rate.
    pub max_busy_writes_per_sec: u64,
    pub policy: PolicyParams,
    pub migrate: MigrateOptions,
}

impl Default for WearOptions {
    fn default() -> Self {
        WearOptions {
            check_interval: Duration::from_secs(60),
            rotate_interval: Duration::from_secs(600),
            hotness_decay_interval: Duration::from_secs(300),
            max_busy_writes_per_sec: 200,
            policy: PolicyParams::default(),
            migrate: MigrateOptions::default(),
        }
    }
}

/// Last few migrations, exposed for `status`.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WearStats {
    pub migrations_done: u64,
    pub migrations_failed: u64,
    pub chunks_retired: u64,
    pub last_report: Option<MigrateReport>,
    pub last_error: Option<String>,
    pub last_check_unix_secs: u64,
}

pub struct WearHandle {
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<WearStats>>,
    thread: Option<JoinHandle<()>>,
}

impl WearHandle {
    pub fn stats(&self) -> WearStats {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Shared handle to the live statistics (for the control socket).
    pub fn stats_arc(&self) -> Arc<Mutex<WearStats>> {
        self.stats.clone()
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WearHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One policy evaluation + execution. Returns `Ok(true)` if a migration ran.
pub fn run_once<B: BlockIo + ?Sized>(
    mapper: &ChunkMapper,
    io: &B,
    signal: &FlushSignal,
    opts: &WearOptions,
    stop: &AtomicBool,
    stats: &Mutex<WearStats>,
) -> Result<bool, MigrateError> {
    let layout = *crate::mapper::Mapper::layout(mapper);
    let excluded = mapper.current_migration().map(|m| m.dst);
    let hot = mapper.hotness_snapshot();
    let (plan, to_retire) = mapper.with_metadata(|md| {
        (
            policy::choose(&layout, md, &hot, &opts.policy, excluded),
            policy::free_chunks_to_retire(&layout, md, &opts.policy),
        )
    });
    for c in to_retire {
        if mapper.retire_free_chunk(c) {
            tracing::warn!(pchunk = c, "retired failing spare chunk");
            stats
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .chunks_retired += 1;
        }
    }
    let Some(plan) = plan else { return Ok(false) };
    tracing::info!(?plan, "starting migration");
    match migrate::migrate(mapper, io, signal, &plan, &opts.migrate, stop) {
        Ok(report) => {
            let mut s = stats.lock().unwrap_or_else(|e| e.into_inner());
            s.migrations_done += 1;
            if plan.reason == Reason::Retire && mapper.retire_free_chunk(report.freed) {
                s.chunks_retired += 1;
                tracing::warn!(
                    pchunk = report.freed,
                    "retired failing chunk after evacuation"
                );
            }
            tracing::info!(?report, "migration done");
            s.last_report = Some(report);
            Ok(true)
        }
        Err(e) => {
            let mut s = stats.lock().unwrap_or_else(|e| e.into_inner());
            s.migrations_failed += 1;
            s.last_error = Some(e.to_string());
            tracing::warn!(error = %e, ?plan, "migration failed");
            Err(e)
        }
    }
}

/// Spawn the worker thread.
pub fn spawn<B: BlockIo + Send + 'static>(
    mapper: Arc<ChunkMapper>,
    io: B,
    signal: Arc<FlushSignal>,
    opts: WearOptions,
) -> WearHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new(WearStats::default()));
    let (stop_t, stats_t) = (stop.clone(), stats.clone());
    let thread = std::thread::Builder::new()
        .name("wlvl-wear".into())
        .spawn(move || worker_loop(&mapper, &io, &signal, &opts, &stop_t, &stats_t))
        .expect("spawning wear thread");
    WearHandle {
        stop,
        stats,
        thread: Some(thread),
    }
}

fn worker_loop<B: BlockIo>(
    mapper: &ChunkMapper,
    io: &B,
    signal: &FlushSignal,
    opts: &WearOptions,
    stop: &AtomicBool,
    stats: &Mutex<WearStats>,
) {
    let tick = Duration::from_millis(500).min(opts.check_interval);
    let mut next_check = Instant::now() + opts.check_interval;
    let mut next_decay = Instant::now() + opts.hotness_decay_interval;
    let mut last_migration: Option<Instant> = None;
    let mut last_writes = (Instant::now(), mapper.writes_total());

    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(tick);
        let now = Instant::now();
        if now >= next_decay {
            mapper.decay_hotness();
            next_decay = now + opts.hotness_decay_interval;
        }
        if now < next_check {
            continue;
        }
        next_check = now + opts.check_interval;
        stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_check_unix_secs = unix_now();

        // Load estimate since the last check.
        let writes = mapper.writes_total();
        let secs = now.duration_since(last_writes.0).as_secs_f64().max(0.001);
        let rate = ((writes - last_writes.1) as f64 / secs) as u64;
        last_writes = (now, writes);
        if rate > opts.max_busy_writes_per_sec {
            tracing::debug!(rate, "device busy; postponing wear check");
            continue;
        }
        if last_migration.is_some_and(|t| now.duration_since(t) < opts.rotate_interval) {
            continue;
        }
        if signal.has_failed() {
            tracing::warn!("metadata persister is failing; skipping migrations");
            continue;
        }
        match run_once(mapper, io, signal, opts, stop, stats) {
            Ok(true) => last_migration = Some(Instant::now()),
            Ok(false) => {}
            Err(MigrateError::Stopped) => break,
            Err(_) => last_migration = Some(Instant::now()),
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
