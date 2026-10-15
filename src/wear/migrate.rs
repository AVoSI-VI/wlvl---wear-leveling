//! Execute one migration: copy, re-copy dirtied ranges, verify, make the new
//! mapping durable, release the old chunk.

use super::policy::Plan;
use crate::mapper::{ChunkMapper, IoOutcome};
use crate::ondisk::BlockIo;
use crate::persist::FlushSignal;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// Copy granularity.
    pub piece_bytes: usize,
    /// Sleep between polls while writes are in flight.
    pub poll_interval: Duration,
    /// How long to wait for the persister to make the switch durable.
    pub commit_timeout: Duration,
    /// After this many copy passes, briefly hold new writes to the chunk so
    /// the final re-copy and switch can complete.
    pub barrier_after_passes: u32,
    /// Hard limit on copy passes before giving up.
    pub max_passes: u32,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        MigrateOptions {
            piece_bytes: 1024 * 1024,
            poll_interval: Duration::from_millis(20),
            commit_timeout: Duration::from_secs(30),
            barrier_after_passes: 6,
            max_passes: 50,
        }
    }
}

#[derive(Debug)]
pub enum MigrateError {
    Io(std::io::Error),
    Mapper(String),
    /// Data read back from the destination did not match while quiescent.
    VerifyMismatch {
        dst: u64,
    },
    Commit(String),
    TooManyPasses,
    Stopped,
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrateError::Io(e) => write!(f, "I/O error: {e}"),
            MigrateError::Mapper(m) => write!(f, "mapper: {m}"),
            MigrateError::VerifyMismatch { dst } => {
                write!(f, "verification failed on physical chunk {dst}")
            }
            MigrateError::Commit(m) => write!(f, "commit: {m}"),
            MigrateError::TooManyPasses => write!(f, "chunk kept changing; gave up"),
            MigrateError::Stopped => write!(f, "stopped"),
        }
    }
}

impl std::error::Error for MigrateError {}

impl From<std::io::Error> for MigrateError {
    fn from(e: std::io::Error) -> Self {
        MigrateError::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MigrateReport {
    pub plan: Plan,
    pub freed: u64,
    pub passes: u32,
    pub bytes_copied: u64,
    pub elapsed_ms: u64,
}

/// Merge overlapping/adjacent ranges and align them to `align` within `limit`.
pub fn coalesce(mut ranges: Vec<(u64, u64)>, align: u64, limit: u64) -> Vec<(u64, u64)> {
    for r in ranges.iter_mut() {
        let start = (r.0 / align) * align;
        let end = ((r.0 + r.1).div_ceil(align) * align).min(limit);
        *r = (start, end - start);
    }
    ranges.retain(|r| r.1 > 0);
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (s, l) in ranges {
        if let Some(last) = out.last_mut() {
            if s <= last.0 + last.1 {
                let end = (s + l).max(last.0 + last.1);
                last.1 = end - last.0;
                continue;
            }
        }
        out.push((s, l));
    }
    out
}

/// Run `plan` to completion. On any failure the mapper is rolled back
/// (destination retired when it was at fault).
pub fn migrate<B: BlockIo + ?Sized>(
    mapper: &ChunkMapper,
    io: &B,
    signal: &FlushSignal,
    plan: &Plan,
    opts: &MigrateOptions,
    stop: &AtomicBool,
) -> Result<MigrateReport, MigrateError> {
    let started = Instant::now();
    let layout = *crate::mapper::Mapper::layout(mapper);
    let cs = layout.chunk_size;
    let piece = (opts.piece_bytes as u64).min(cs).max(4096);
    let info = mapper
        .begin_migration(plan.lchunk, plan.dst)
        .map_err(MigrateError::Mapper)?;
    let src_off = layout.chunk_offset(info.src);
    let dst_off = layout.chunk_offset(info.dst);

    let mut buf = vec![0u8; piece as usize];
    let mut check = vec![0u8; piece as usize];
    let mut ranges = vec![(0u64, cs)];
    let mut passes = 0u32;
    let mut bytes_copied = 0u64;

    let result: Result<(), MigrateError> = loop {
        if stop.load(Ordering::SeqCst) {
            break Err(MigrateError::Stopped);
        }
        passes += 1;
        if passes > opts.max_passes {
            break Err(MigrateError::TooManyPasses);
        }
        if passes == opts.barrier_after_passes + 1 {
            tracing::debug!(
                lchunk = plan.lchunk,
                passes,
                "chunk stays hot; raising write barrier for final switch"
            );
            mapper.raise_barrier().map_err(MigrateError::Mapper)?;
        }
        let mut suspect: Vec<(u64, u64)> = Vec::new();
        for (start, len) in coalesce(ranges, piece, cs) {
            let mut off = start;
            while off < start + len {
                let n = piece.min(start + len - off) as usize;
                let (b, c) = (&mut buf[..n], &mut check[..n]);
                io.read_at(b, src_off + off)?;
                let t = Instant::now();
                let w = io.write_at(b, dst_off + off);
                mapper.record_background_write(
                    info.dst,
                    IoOutcome {
                        latency: t.elapsed(),
                        ok: w.is_ok(),
                    },
                );
                w?;
                bytes_copied += n as u64;
                io.read_at(c, dst_off + off)?;
                if b != c {
                    suspect.push((off, n as u64));
                }
                off += n as u64;
            }
        }
        if !suspect.is_empty() {
            // A mismatch is only a fault if no write could have raced with us.
            let (dirty, inflight) = mapper.drain_dirty().map_err(MigrateError::Mapper)?;
            if dirty.is_empty() && inflight == 0 {
                mapper.record_verify_failure(info.dst);
                break Err(MigrateError::VerifyMismatch { dst: info.dst });
            }
            ranges = dirty;
            ranges.extend(suspect);
            continue;
        }
        match mapper.finish_copy().map_err(MigrateError::Mapper)? {
            Ok(()) => break Ok(()),
            Err(dirty) if dirty.is_empty() => {
                std::thread::sleep(opts.poll_interval);
                ranges = Vec::new();
                passes -= 1; // waiting for in-flight writes is not a copy pass
            }
            Err(dirty) => ranges = dirty,
        }
    };

    if let Err(e) = result {
        mapper.abort_migration(matches!(e, MigrateError::VerifyMismatch { .. }));
        return Err(e);
    }

    // Committing phase: make the switch durable while still mirroring.
    io.sync()?;
    let ticket = signal.request_urgent();
    if let Err(e) = signal.wait_acked(ticket, opts.commit_timeout) {
        mapper.abort_migration(false);
        return Err(MigrateError::Commit(e));
    }
    let freed = mapper.complete_migration().map_err(MigrateError::Mapper)?;
    Ok(MigrateReport {
        plan: *plan,
        freed,
        passes,
        bytes_copied,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_merges_and_aligns() {
        let r = coalesce(vec![(5000, 100), (4096, 10), (20000, 1)], 4096, 1 << 20);
        assert_eq!(r, vec![(4096, 4096), (16384, 4096)]);
        let r = coalesce(vec![(20000, 1), (20480, 1)], 4096, 1 << 20);
        assert_eq!(r, vec![(16384, 8192)]);
        let r = coalesce(vec![(0, 4096), (4096, 4096)], 4096, 1 << 20);
        assert_eq!(r, vec![(0, 8192)]);
        let r = coalesce(vec![((1 << 20) - 10, 100)], 4096, 1 << 20);
        assert_eq!(r, vec![((1 << 20) - 4096, 4096)]);
        assert!(coalesce(vec![], 4096, 1 << 20).is_empty());
    }
}
