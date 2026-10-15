//! Health inferred from our own per-chunk counters.
//!
//! SD cards and USB sticks hide their flash behind a controller, so this is an
//! estimate: write counts against an expected endurance, plus error, verify
//! and latency signals.

use crate::ondisk::{ChunkState, ChunkStats, Layout, Metadata};
use crate::wear::policy::{summarize, WearSummary};

#[derive(Debug, Clone)]
pub struct HealthParams {
    /// Program/erase cycles the flash is expected to survive (TLC default).
    pub expected_pe_cycles: u64,
    /// Number of histogram buckets.
    pub histogram_buckets: usize,
    /// How many of the worst chunks to list.
    pub worst_count: usize,
}

impl Default for HealthParams {
    fn default() -> Self {
        HealthParams {
            expected_pe_cycles: 3000,
            histogram_buckets: 10,
            worst_count: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistBucket {
    pub lo: u64,
    pub hi: u64,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChunkDetail {
    pub pchunk: u64,
    pub score: u8,
    pub state: ChunkState,
    pub write_count: u64,
    pub io_errors: u16,
    pub verify_failures: u16,
    pub latency_ewma_us: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LatencySummary {
    pub min_us: u32,
    pub mean_us: u32,
    pub max_us: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HealthReport {
    /// 0 (dead) .. 100 (new).
    pub device_score: u8,
    pub min_chunk_score: u8,
    pub mean_chunk_score: u8,
    /// Mean chunk writes as a percentage of expected endurance (x100 for two decimals).
    pub endurance_used_centi_pct: u64,
    pub wear: WearSummary,
    pub chunks_with_errors: u64,
    pub chunks_with_verify_failures: u64,
    pub latency: LatencySummary,
    pub histogram: Vec<HistBucket>,
    pub worst_chunks: Vec<ChunkDetail>,
    pub note: String,
}

/// Score one chunk. `latency_baseline_us` is the pool median EWMA.
pub fn chunk_score(st: &ChunkStats, p: &HealthParams, latency_baseline_us: u32) -> u8 {
    if st.state == ChunkState::Retired {
        return 0;
    }
    let mut score: i64 = 100;
    let wear_pen =
        (70 * st.write_count as i128 / p.expected_pe_cycles.max(1) as i128).min(70) as i64;
    score -= wear_pen;
    score -= (15 * st.io_errors as i64).min(40);
    score -= (10 * st.verify_failures as i64).min(30);
    if latency_baseline_us > 0 && st.latency_ewma_us > latency_baseline_us.saturating_mul(3) {
        score -= 10;
    }
    score.clamp(0, 100) as u8
}

pub fn assess(layout: &Layout, md: &Metadata, p: &HealthParams) -> HealthReport {
    let wear = summarize(layout, md);
    let pool: Vec<(u64, &ChunkStats)> = md
        .stats
        .iter()
        .enumerate()
        .map(|(i, s)| (i as u64, s))
        .filter(|(i, s)| !layout.is_reserved(*i) && s.state != ChunkState::Retired)
        .collect();

    let mut lat: Vec<u32> = pool
        .iter()
        .map(|(_, s)| s.latency_ewma_us)
        .filter(|&l| l > 0)
        .collect();
    lat.sort_unstable();
    let baseline = lat.get(lat.len() / 2).copied().unwrap_or(0);
    let latency = if lat.is_empty() {
        LatencySummary {
            min_us: 0,
            mean_us: 0,
            max_us: 0,
        }
    } else {
        let sum: u64 = lat.iter().map(|&l| l as u64).sum();
        LatencySummary {
            min_us: lat[0],
            mean_us: (sum / lat.len() as u64) as u32,
            max_us: *lat.last().unwrap(),
        }
    };

    let mut details: Vec<ChunkDetail> = pool
        .iter()
        .map(|(i, s)| ChunkDetail {
            pchunk: *i,
            score: chunk_score(s, p, baseline),
            state: s.state,
            write_count: s.write_count,
            io_errors: s.io_errors,
            verify_failures: s.verify_failures,
            latency_ewma_us: s.latency_ewma_us,
        })
        .collect();

    let (min_score, mean_score) = if details.is_empty() {
        (0, 0)
    } else {
        let min = details.iter().map(|d| d.score).min().unwrap();
        let sum: u64 = details.iter().map(|d| d.score as u64).sum();
        (min, (sum / details.len() as u64) as u8)
    };
    let device_score = ((min_score as u32 * 3 + mean_score as u32 * 7) / 10) as u8;

    let chunks_with_errors = details.iter().filter(|d| d.io_errors > 0).count() as u64;
    let chunks_with_verify_failures =
        details.iter().filter(|d| d.verify_failures > 0).count() as u64;
    let endurance_used_centi_pct = wear.mean * 10_000 / p.expected_pe_cycles.max(1);

    let histogram = histogram(&details, wear.min, wear.max, p.histogram_buckets);
    details.sort_by_key(|d| (d.score, std::cmp::Reverse(d.write_count)));
    details.truncate(p.worst_count);

    HealthReport {
        device_score,
        min_chunk_score: min_score,
        mean_chunk_score: mean_score,
        endurance_used_centi_pct,
        wear,
        chunks_with_errors,
        chunks_with_verify_failures,
        latency,
        histogram,
        worst_chunks: details,
        note: "inferred from host-side counters; the card's internal wear is not visible".into(),
    }
}

fn histogram(details: &[ChunkDetail], min: u64, max: u64, buckets: usize) -> Vec<HistBucket> {
    let buckets = buckets.max(1);
    let span = (max - min).max(1);
    let width = span.div_ceil(buckets as u64).max(1);
    let mut out: Vec<HistBucket> = (0..buckets as u64)
        .map(|b| HistBucket {
            lo: min + b * width,
            hi: min + (b + 1) * width,
            count: 0,
        })
        .collect();
    for d in details {
        let idx = (((d.write_count - min) / width) as usize).min(buckets - 1);
        out[idx].count += 1;
    }
    while out.len() > 1 && out.last().is_some_and(|b| b.count == 0 && b.lo > max) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::layout::MIB;

    fn setup() -> (Layout, Metadata) {
        let layout = Layout::compute(256 * MIB, 4 * MIB, 4, 10).unwrap();
        (layout, Metadata::initial(&layout))
    }

    #[test]
    fn new_device_scores_100() {
        let (layout, md) = setup();
        let r = assess(&layout, &md, &HealthParams::default());
        assert_eq!(
            (r.device_score, r.min_chunk_score, r.mean_chunk_score),
            (100, 100, 100)
        );
        assert_eq!(r.endurance_used_centi_pct, 0);
        assert_eq!(
            r.histogram.iter().map(|b| b.count).sum::<u64>(),
            layout.pool_chunks()
        );
    }

    #[test]
    fn wear_errors_and_retirement_reduce_scores() {
        let (layout, mut md) = setup();
        let p = HealthParams::default();
        let a = md.physical_of(0) as usize;
        md.stats[a].write_count = 1500; // half the endurance -> -35
        assert_eq!(chunk_score(&md.stats[a], &p, 0), 65);
        let b = md.physical_of(1) as usize;
        md.stats[b].io_errors = 2;
        md.stats[b].verify_failures = 1;
        assert_eq!(chunk_score(&md.stats[b], &p, 0), 60);
        let c = md.physical_of(2) as usize;
        md.stats[c].state = ChunkState::Retired;
        assert_eq!(chunk_score(&md.stats[c], &p, 0), 0);
        let d = md.physical_of(3) as usize;
        md.stats[d].latency_ewma_us = 5000;
        assert_eq!(chunk_score(&md.stats[d], &p, 1000), 90);
        assert_eq!(chunk_score(&md.stats[d], &p, 0), 100);

        let r = assess(&layout, &md, &p);
        assert!(r.device_score < 100 && r.min_chunk_score == 60);
        assert_eq!(r.wear.retired_chunks, 1);
        assert_eq!(r.chunks_with_errors, 1);
        assert_eq!(r.worst_chunks[0].pchunk as usize, b);
        // The retired chunk leaves the pool, so the mean is over pool_chunks - 1.
        assert_eq!(r.wear.pool_chunks, layout.pool_chunks() - 1);
        assert_eq!(
            r.endurance_used_centi_pct,
            (1500 / r.wear.pool_chunks) * 10_000 / 3000
        );
    }

    #[test]
    fn histogram_covers_range() {
        let (layout, mut md) = setup();
        for (i, l) in (0..layout.data_chunks).enumerate() {
            let p = md.physical_of(l) as usize;
            md.stats[p].write_count = i as u64 * 10;
        }
        let r = assess(&layout, &md, &HealthParams::default());
        assert_eq!(r.histogram.len(), 10);
        assert_eq!(
            r.histogram.iter().map(|b| b.count).sum::<u64>(),
            layout.pool_chunks()
        );
        assert_eq!(r.histogram[0].lo, 0);
    }
}
