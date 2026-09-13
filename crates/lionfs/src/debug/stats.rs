//! Aggregated filesystem statistics for `tools::report`/`tools::health`,
//! combining `fs::stat` (usage) and `allocator::statistics`
//! (fragmentation) into one struct so a tool doesn't need to know about
//! both modules separately.

use crate::allocator::statistics::{analyze, FragmentationStats};
use crate::fs::stat::{compute_stats, FsStats};
use crate::ondisk::serialization::Superblock;

// ---------------------------------------------------------------------------
// Phase 2: RAID parity-write alignment counters.
//
// `write_block_parity` (the RAID5/6 write path) must read the rest of
// the stripe row to recompute parity whenever the write does not cover
// a complete chunk. These counters measure how often that happens, so
// the cost of partial-chunk writes is a measured number rather than a
// guess (see docs/benchmarks.md for the numbers that motivated Phase
// 3's incremental parity update).
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};

pub static PARITY_WRITES_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Parity writes whose written range covers a partial chunk (the write
/// starts mid-chunk, or is shorter than a full chunk) and therefore
/// forces reading the stripe row's other data blocks for a full parity
/// recompute.
pub static PARITY_WRITES_PARTIAL_CHUNK: AtomicU64 = AtomicU64::new(0);
/// Total "other data" blocks read by the parity path (the actual I/O
/// amplification paid by full-row recomputes).
pub static PARITY_ROW_READS: AtomicU64 = AtomicU64::new(0);
/// Parity writes served by the Phase 3 incremental (RMW) path.
pub static PARITY_INCREMENTAL_UPDATES: AtomicU64 = AtomicU64::new(0);
/// Incremental attempts that fell back to the full recompute (degraded
/// pool / unreadable old parity).
pub static PARITY_INCREMENTAL_FALLBACKS: AtomicU64 = AtomicU64::new(0);

pub fn parity_alignment_report() -> String {
    let total = PARITY_WRITES_TOTAL.load(Ordering::Relaxed);
    let partial = PARITY_WRITES_PARTIAL_CHUNK.load(Ordering::Relaxed);
    let row_reads = PARITY_ROW_READS.load(Ordering::Relaxed);
    if total == 0 {
        return "parity writes: none recorded".to_string();
    }
    format!(
        "parity writes: {} total, {} partial-chunk ({:.1}%), {} stripe-row reads ({:.2} per write)",
        total,
        partial,
        100.0 * partial as f64 / total as f64,
        row_reads,
        row_reads as f64 / total as f64
    )
}

pub fn reset_parity_counters() {
    PARITY_WRITES_TOTAL.store(0, Ordering::Relaxed);
    PARITY_WRITES_PARTIAL_CHUNK.store(0, Ordering::Relaxed);
    PARITY_ROW_READS.store(0, Ordering::Relaxed);
    PARITY_INCREMENTAL_UPDATES.store(0, Ordering::Relaxed);
    PARITY_INCREMENTAL_FALLBACKS.store(0, Ordering::Relaxed);
}

#[derive(Debug, Clone)]
pub struct AggregateStats {
    pub usage: FsStats,
    pub fragmentation: FragmentationStats,
}

pub fn collect(sb: &Superblock, free_extents: &[(u64, u64)]) -> AggregateStats {
    AggregateStats {
        usage: compute_stats(sb),
        fragmentation: analyze(free_extents),
    }
}

pub fn format_report(stats: &AggregateStats) -> String {
    format!(
        "Total blocks:      {}\nFree blocks:       {}\nUsed:              {:.1}%\nFree extents:      {}\nLargest free run:  {} blocks\nFragmentation:     {:.1}%",
        stats.usage.total_blocks,
        stats.usage.free_blocks,
        crate::fs::stat::used_fraction(&stats.usage) * 100.0,
        stats.fragmentation.free_extent_count,
        stats.fragmentation.largest_free_extent,
        stats.fragmentation.fragmentation_ratio * 100.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn report_contains_key_figures() {
        let mut sb = Superblock::zeroed();
        sb.total_blocks = 1000;
        sb.free_blocks = 250;
        let stats = collect(&sb, &[(0, 100), (500, 150)]);
        let report = format_report(&stats);
        assert!(report.contains("1000"));
        assert!(report.contains("75.0%")); // used fraction
    }
}
