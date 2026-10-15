//! Selection of the next migration: which logical chunk moves where, and why.
//!
//! Pure functions over a metadata snapshot so the policy is unit-testable.

use crate::ondisk::{ChunkState, Layout, Metadata};

#[derive(Debug, Clone)]
pub struct PolicyParams {
    /// Act only when `max_wear - min_wear` over the pool exceeds this.
    pub wear_delta_threshold: u64,
    /// Chunks with at least this many I/O errors are evacuated and retired.
    pub retire_threshold: u16,
    /// Chunks with at least this many verification failures are retired too.
    pub verify_retire_threshold: u16,
    /// A logical chunk needs at least this much heat to count as "hot".
    pub min_hotness: u32,
}

impl Default for PolicyParams {
    fn default() -> Self {
        PolicyParams {
            wear_delta_threshold: 50,
            retire_threshold: 3,
            verify_retire_threshold: 2,
            min_hotness: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Source chunk is failing; move the data off it and retire it.
    Retire,
    /// Hot data sits on a worn chunk; move it to a fresh one.
    Dynamic,
    /// Cold data sits on a fresh chunk; move it to a worn one to free the fresh chunk.
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    pub lchunk: u64,
    pub src: u64,
    pub dst: u64,
    pub reason: Reason,
}

/// Summary of pool wear used by `choose` and by status output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct WearSummary {
    pub min: u64,
    pub max: u64,
    pub mean: u64,
    pub pool_chunks: u64,
    pub free_chunks: u64,
    pub retired_chunks: u64,
}

pub fn summarize(layout: &Layout, md: &Metadata) -> WearSummary {
    let mut s = WearSummary {
        min: u64::MAX,
        ..Default::default()
    };
    let mut total: u128 = 0;
    for (p, st) in md.stats.iter().enumerate() {
        if layout.is_reserved(p as u64) {
            continue;
        }
        match st.state {
            ChunkState::Retired => s.retired_chunks += 1,
            ChunkState::Free | ChunkState::InUse => {
                if st.state == ChunkState::Free {
                    s.free_chunks += 1;
                }
                s.pool_chunks += 1;
                s.min = s.min.min(st.write_count);
                s.max = s.max.max(st.write_count);
                total += st.write_count as u128;
            }
            ChunkState::Reserved => {}
        }
    }
    if s.pool_chunks == 0 {
        s.min = 0;
    } else {
        s.mean = (total / s.pool_chunks as u128) as u64;
    }
    s
}

fn failing(st: &crate::ondisk::ChunkStats, p: &PolicyParams) -> bool {
    st.io_errors >= p.retire_threshold || st.verify_failures >= p.verify_retire_threshold
}

/// Free chunks that should be retired outright (they already fail).
pub fn free_chunks_to_retire(layout: &Layout, md: &Metadata, p: &PolicyParams) -> Vec<u64> {
    md.free_chunks()
        .filter(|&c| !layout.is_reserved(c) && failing(&md.stats[c as usize], p))
        .collect()
}

/// Pick the next migration, if any. `excluded_dst` is a chunk that must not be
/// chosen as destination (e.g. one currently being migrated into).
pub fn choose(
    layout: &Layout,
    md: &Metadata,
    hotness: &[u32],
    p: &PolicyParams,
    excluded_dst: Option<u64>,
) -> Option<Plan> {
    let free: Vec<u64> = md
        .free_chunks()
        .filter(|&c| Some(c) != excluded_dst && !failing(&md.stats[c as usize], p))
        .collect();
    if free.is_empty() {
        return None;
    }
    let wear = |c: u64| md.stats[c as usize].write_count;
    let freshest = *free.iter().min_by_key(|&&c| wear(c))?;
    let most_worn = *free.iter().max_by_key(|&&c| wear(c))?;

    // 1. Evacuate failing chunks.
    if let Some((l, &src)) = md
        .mapping
        .iter()
        .enumerate()
        .map(|(l, s)| (l as u64, s))
        .find(|(_, &s)| failing(&md.stats[s as usize], p))
    {
        return Some(Plan {
            lchunk: l,
            src: src as u64,
            dst: freshest,
            reason: Reason::Retire,
        });
    }

    let summary = summarize(layout, md);
    if summary.max.saturating_sub(summary.min) <= p.wear_delta_threshold {
        return None;
    }
    let half = p.wear_delta_threshold / 2;

    // 2. Dynamic: hottest logical chunk sitting above the mean wear.
    let dynamic = md
        .mapping
        .iter()
        .enumerate()
        .map(|(l, &s)| (l as u64, s as u64))
        .filter(|&(l, s)| {
            hotness.get(l as usize).copied().unwrap_or(0) >= p.min_hotness && wear(s) > summary.mean
        })
        .max_by_key(|&(l, s)| (hotness[l as usize], wear(s)));
    if let Some((l, src)) = dynamic {
        if wear(src) > wear(freshest) + half {
            return Some(Plan {
                lchunk: l,
                src,
                dst: freshest,
                reason: Reason::Dynamic,
            });
        }
    }

    // 3. Static: coldest logical chunk on the least-worn in-use chunk.
    let statik = md
        .mapping
        .iter()
        .enumerate()
        .map(|(l, &s)| (l as u64, s as u64))
        .min_by_key(|&(l, s)| (hotness.get(l as usize).copied().unwrap_or(0), wear(s)));
    if let Some((l, src)) = statik {
        if wear(most_worn) > wear(src) + half {
            return Some(Plan {
                lchunk: l,
                src,
                dst: most_worn,
                reason: Reason::Static,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::layout::MIB;

    fn setup() -> (Layout, Metadata) {
        let layout = Layout::compute(256 * MIB, 4 * MIB, 4, 10).unwrap();
        let md = Metadata::initial(&layout);
        (layout, md)
    }

    #[test]
    fn balanced_pool_yields_nothing() {
        let (layout, md) = setup();
        let hot = vec![0; layout.data_chunks as usize];
        assert_eq!(
            choose(&layout, &md, &hot, &PolicyParams::default(), None),
            None
        );
        let s = summarize(&layout, &md);
        assert_eq!((s.min, s.max, s.free_chunks), (0, 0, layout.spare_chunks));
    }

    #[test]
    fn dynamic_moves_hot_worn_chunk_to_freshest_spare() {
        let (layout, mut md) = setup();
        let mut hot = vec![0u32; layout.data_chunks as usize];
        let src = md.physical_of(7);
        md.stats[src as usize].write_count = 500;
        hot[7] = 100;
        let plan = choose(&layout, &md, &hot, &PolicyParams::default(), None).unwrap();
        assert_eq!(plan.reason, Reason::Dynamic);
        assert_eq!((plan.lchunk, plan.src), (7, src));
        assert_eq!(md.stats[plan.dst as usize].state, ChunkState::Free);
        // Excluding that destination picks another free chunk.
        let plan2 = choose(&layout, &md, &hot, &PolicyParams::default(), Some(plan.dst)).unwrap();
        assert_ne!(plan2.dst, plan.dst);
    }

    #[test]
    fn static_moves_cold_fresh_chunk_onto_worn_spare() {
        let (layout, mut md) = setup();
        let hot = vec![0u32; layout.data_chunks as usize];
        let worn_free = md.free_chunks().next().unwrap();
        md.stats[worn_free as usize].write_count = 400;
        let plan = choose(&layout, &md, &hot, &PolicyParams::default(), None).unwrap();
        assert_eq!(plan.reason, Reason::Static);
        assert_eq!(plan.dst, worn_free);
    }

    #[test]
    fn failing_chunks_take_priority() {
        let (layout, mut md) = setup();
        let hot = vec![0u32; layout.data_chunks as usize];
        let src = md.physical_of(3);
        md.stats[src as usize].io_errors = 3;
        let plan = choose(&layout, &md, &hot, &PolicyParams::default(), None).unwrap();
        assert_eq!((plan.reason, plan.lchunk), (Reason::Retire, 3));
        let bad_free = md.free_chunks().last().unwrap();
        md.stats[bad_free as usize].verify_failures = 2;
        assert_eq!(
            free_chunks_to_retire(&layout, &md, &PolicyParams::default()),
            vec![bad_free]
        );
        let plan = choose(&layout, &md, &hot, &PolicyParams::default(), None).unwrap();
        assert_ne!(plan.dst, bad_free);
    }

    #[test]
    fn no_free_chunks_means_no_plan() {
        let (layout, mut md) = setup();
        for c in md.free_chunks().collect::<Vec<_>>() {
            md.stats[c as usize].state = ChunkState::Retired;
        }
        let p0 = md.physical_of(0) as usize;
        md.stats[p0].write_count = 1000;
        let hot = vec![10u32; layout.data_chunks as usize];
        assert_eq!(
            choose(&layout, &md, &hot, &PolicyParams::default(), None),
            None
        );
    }
}
