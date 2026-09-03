//! # Copy-GC & Space Reclamation (RFC-004 §6)
//!
//! CoW buys crash consistency and snapshots; the price is *stale
//! extents*. Every overwrite leaves the old version of a block behind
//! until its refcount drops to zero, and a filesystem that never
//! reclaims them simply leaks capacity at the CoW rate. The 1.x line
//! relied on the refcount tables plus "the GC worker will handle it"
//! (a comment in `fs/snapshots.rs`, not a worker); RFC-004 §6 makes
//! the worker real.
//!
//! The policy is the classic Rosenblum-Ousterhout cost/benefit
//! selection, extended for the media LionFS actually runs on:
//!
//! * **benefit of reclaiming segment s** = `freeable(s) * age(s)` --
//!   freeable bytes times time-since-write. The age factor is the
//!   cold-data prior: a segment whose live bytes were written long ago
//!   is likely to *stay* live (copying it buys durable free space);
//!   a hot segment will free itself through ordinary churn, so
//!   spending copy IO on it is waste.
//! * **cost of reclaiming s** = `2 * live(s) * (1 + wear_penalty(s))`:
//!   read live bytes + write them elsewhere, with a wear-leveling
//!   penalty that steers selection *away* from heavily-written
//!   segments on flash/ZNS media (picking the same hot segment
//!   forever is how you pin its write endurance).
//! * **score** = `benefit / cost`, picked highest-first.
//!
//! Watermarks: the planner produces nothing while free-space is above
//! `kick` (default 20%); produces a background trickle between `kick`
//! and `aggressive` (default 8%); above `aggressive`, selection
//! switches to *panic mode* -- score ordering degrades to pure
//! freeable-bytes ordering, because at 8% free the correct move is to
//! reclaim *now*, not to be clever.
//!
//! This module is the policy/planning layer: it emits relocation plans
//! (which segments, which live extents in them, estimated copy and
//! reclaimed bytes) that the transaction layer executes through the
//! ordinary CoW write path, so a GC relocation is indistinguishable
//! from a user write to every layer below it -- it gets checksummed,
//! journaled, and crash-recovered identically. GC IO runs in the
//! `Bulk` QoS class (RFC-004 §4.1), which is what keeps scrub/GC from
//! stealing latency from tenants.

use std::time::Duration;

/// Default free-space fraction at which background GC kicks in.
pub const DEFAULT_KICK_PCT: u8 = 20;
/// Default free-space fraction at which GC becomes aggressive.
pub const DEFAULT_AGGRESSIVE_PCT: u8 = 8;

/// One reclaimable unit: a zone (ZNS), band (SMR), or region slice
/// (conventional). All fields are caller-supplied; the planner is
/// media-agnostic on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentStat {
    pub segment_id: u64,
    /// Bytes the segment holds, live + stale.
    pub total_bytes: u64,
    /// Bytes still referenced (refcount > 0).
    pub live_bytes: u64,
    /// Time since the newest write landed in this segment (ns).
    pub age_ns: u64,
    /// Lifetime write cycles this segment has seen (wear proxy).
    pub write_cycles: u64,
}

impl SegmentStat {
    /// Bytes reclaimable by evacuating the segment.
    #[must_use]
    pub fn freeable_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.live_bytes)
    }

    /// Fraction live, in basis points (0..=10_000) -- integer math on
    /// purpose, shared bit-for-bit between simulator and engine.
    #[must_use]
    pub fn live_bps(&self) -> u64 {
        if self.total_bytes == 0 {
            return 0;
        }
        (self.live_bytes.saturating_mul(10_000)) / self.total_bytes
    }
}

/// Tunables for the cost model.
#[derive(Clone, Copy, Debug)]
pub struct GcConfig {
    /// Free % at which background GC starts.
    pub kick_pct: u8,
    /// Free % at which GC becomes aggressive (panic mode).
    pub aggressive_pct: u8,
    /// Wear penalty per 100 write cycles, in basis points of cost
    /// inflation (100 bps = +1% cost per 100 cycles).
    pub wear_bps_per_100_cycles: u64,
    /// Age prior weight: benefit multiplier is
    /// `1 + age_ns / age_half_life_ns`, so a segment older than the
    /// half-life is worth 2x the benefit of a fresh one.
    pub age_half_life_ns: u64,
    /// Maximum segments per plan (bounds each batch's copy IO).
    pub max_segments_per_plan: usize,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            kick_pct: DEFAULT_KICK_PCT,
            aggressive_pct: DEFAULT_AGGRESSIVE_PCT,
            wear_bps_per_100_cycles: 5,
            // One week half-life: snapshots/CoW churn is daily-ish.
            age_half_life_ns: Duration::from_secs(7 * 24 * 3600).as_nanos() as u64,
            max_segments_per_plan: 8,
        }
    }
}

/// How urgent the plan is (drives QoS class and rate).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GcUrgency {
    /// Free space healthy: no plan.
    Idle,
    /// Background trickle: Bulk class, rate-limited.
    Background,
    /// Panic mode: freeable-bytes ordering, no rate limit, Realtime
    /// preemption still applies (user IO wins the queue, always).
    Aggressive,
}

/// A relocation plan: the segments to evacuate, in execution order,
/// with cost accounting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcPlan {
    pub urgency: GcUrgency,
    /// Segment ids to evacuate, best-score-first (or biggest-freeable
    /// first in panic mode).
    pub segments: Vec<u64>,
    /// Estimated bytes the executor must copy (2x live: read+write).
    pub estimated_copy_bytes: u64,
    /// Estimated bytes reclaimed once the segments are reset.
    pub estimated_reclaimed_bytes: u64,
}

/// The planner: pure function of `(stats, total_pool_bytes, free_bytes,
/// config) -> Option<GcPlan>`.
#[derive(Clone, Debug, Default)]
pub struct GcPlanner {
    config: GcConfig,
}

impl GcPlanner {
    #[must_use]
    pub fn new(config: GcConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn config(&self) -> &GcConfig {
        &self.config
    }

    /// The benefit/cost score of evacuating one segment (higher =
    /// better). Panic mode (`aggressive = true`) short-circuits to
    /// pure freeable bytes.
    #[must_use]
    pub fn score(&self, s: &SegmentStat, aggressive: bool) -> u64 {
        let freeable = s.freeable_bytes();
        if freeable == 0 {
            return 0; // nothing to gain; never pick
        }
        if aggressive {
            return freeable;
        }
        // Benefit: freeable * age multiplier, all in 32.32 via u128.
        let age_mult: u128 = 1 << 32; // 1.0
        let half_life = (self.config.age_half_life_ns.max(1)) as u128;
        let age_num: u128 =
            age_mult.saturating_add((s.age_ns as u128 * age_mult) / half_life);
        let benefit: u128 = (u128::from(freeable) * age_num) >> 32;
        // Cost: 2 * live * (1 + wear_bps/1e4).
        let wear_bps: u64 = s
            .write_cycles
            .saturating_mul(self.config.wear_bps_per_100_cycles)
            / 100;
        let cost_mult: u128 = (1u128 << 32) + ((u128::from(wear_bps) << 32) / 10_000);
        let cost: u128 = (2 * u128::from(s.live_bytes) * cost_mult) >> 32;
        // score = benefit / cost, scaled to integer u64. cost == 0 (an
        // entirely-dead segment) is maximal.
        if cost == 0 {
            return u64::MAX / 2; // leave headroom so tiebreaks stay sane
        }
        let scaled = (benefit << 20) / cost;
        u64::try_from(scaled.min(u128::from(u64::MAX / 2))).unwrap_or(u64::MAX / 2)
    }

    /// Plans the next batch. `pool_total_bytes`/`pool_free_bytes`
    /// decide urgency; `stats` is the segment census.
    #[must_use]
    pub fn plan(&self, stats: &[SegmentStat], pool_total_bytes: u64, pool_free_bytes: u64) -> Option<GcPlan> {
        if pool_total_bytes == 0 {
            return None;
        }
        let free_bps = (pool_free_bytes.saturating_mul(10_000)) / pool_total_bytes;
        let free_pct = (free_bps / 100) as u8;
        let urgency = if free_pct >= self.config.kick_pct {
            return None; // Idle: healthy, no work
        } else if free_pct < self.config.aggressive_pct {
            GcUrgency::Aggressive
        } else {
            GcUrgency::Background
        };
        let aggressive = urgency == GcUrgency::Aggressive;

        // Score every segment, keep the best max_segments_per_plan.
        let mut ranked: Vec<(u64, SegmentStat)> = stats
            .iter()
            .filter(|s| s.freeable_bytes() > 0)
            .map(|s| (self.score(s, aggressive), *s))
            .collect();
        if ranked.is_empty() {
            return None; // nothing reclaimable anywhere (report: pool full of live data)
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.segment_id.cmp(&b.1.segment_id)));
        ranked.truncate(self.config.max_segments_per_plan);

        let mut copy = 0u64;
        let mut reclaimed = 0u64;
        let mut ids = Vec::with_capacity(ranked.len());
        for (_, s) in &ranked {
            copy = copy.saturating_add(2 * s.live_bytes);
            reclaimed = reclaimed.saturating_add(s.freeable_bytes());
            ids.push(s.segment_id);
        }
        Some(GcPlan {
            urgency,
            segments: ids,
            estimated_copy_bytes: copy,
            estimated_reclaimed_bytes: reclaimed,
        })
    }
}

/// A single stale-extent reclamation record: what the refcount
/// subsystem hands the GC when an extent's last reference drops.
///
/// The 1.x `integrity/refcount.rs` decrements eagerly; this record is
/// the *accounting* side of that event (which segment gained freeable
/// bytes), so the planner's next census sees the release without a
/// device-wide scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReclaimEvent {
    pub segment_id: u64,
    pub freed_bytes: u64,
    /// When the release happened (ns) -- the age anchor for the
    /// segment's *stale* population.
    pub at_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3600 * 1_000_000_000;
    const DAY: u64 = 24 * HOUR;
    const SEG: u64 = 256 << 20; // 256 MiB segments

    fn seg(id: u64, live: u64, age_ns: u64, cycles: u64) -> SegmentStat {
        SegmentStat {
            segment_id: id,
            total_bytes: SEG,
            live_bytes: live,
            age_ns,
            write_cycles: cycles,
        }
    }

    #[test]
    fn freeable_and_live_bps() {
        let s = seg(1, SEG / 4, 0, 0);
        assert_eq!(s.freeable_bytes(), 3 * SEG / 4);
        assert_eq!(s.live_bps(), 2_500);
        assert_eq!(seg(1, 0, 0, 0).live_bps(), 0);
        // Degenerate: zero-size segment is all-live-zero, not a div-by-zero.
        assert_eq!(
            SegmentStat { segment_id: 9, total_bytes: 0, live_bytes: 0, age_ns: 0, write_cycles: 0 }.live_bps(),
            0
        );
    }

    #[test]
    fn healthy_pool_plans_nothing() {
        let p = GcPlanner::default();
        let stats = vec![seg(1, 0, DAY, 0)];
        assert!(p.plan(&stats, 100 * SEG, 50 * SEG).is_none()); // 50% free
    }

    #[test]
    fn background_mode_between_watermarks() {
        let p = GcPlanner::default(); // kick 20%, aggressive 8%
        let stats = vec![seg(1, SEG / 2, DAY, 0)];
        let plan = p.plan(&stats, 100 * SEG, 15 * SEG).expect("15% free -> background");
        assert_eq!(plan.urgency, GcUrgency::Background);
        assert_eq!(plan.segments, vec![1]);
        assert_eq!(plan.estimated_reclaimed_bytes, SEG / 2);
        assert_eq!(plan.estimated_copy_bytes, SEG); // 2x live
    }

    #[test]
    fn panic_mode_below_aggressive_watermark() {
        let p = GcPlanner::default();
        // Two candidates: an old 90%-dead segment and a young 10%-dead
        // one. Panic mode must pick by pure freeable bytes.
        let old = seg(1, SEG / 10, 30 * DAY, 0);
        let young_dead = seg(2, 9 * SEG / 10, HOUR, 0);
        let plan = p
            .plan(&[young_dead, old], 100 * SEG, 5 * SEG)
            .expect("5% free -> aggressive");
        assert_eq!(plan.urgency, GcUrgency::Aggressive);
        assert_eq!(plan.segments.first().copied(), Some(1)); // most freeable first
    }

    #[test]
    fn age_prior_prefers_cold_segments() {
        let p = GcPlanner::default();
        // Identical freeable/live, different ages: the older one must
        // score higher.
        let cold = seg(1, SEG / 2, 30 * DAY, 0);
        let hot = seg(2, SEG / 2, HOUR, 0);
        assert!(p.score(&cold, false) > p.score(&hot, false));
    }

    #[test]
    fn wear_penalty_favors_undamaged_segments() {
        let p = GcPlanner::default();
        let fresh = seg(1, SEG / 2, DAY, 0);
        let worn = seg(2, SEG / 2, DAY, 5_000); // 5000 cycles
        assert!(p.score(&fresh, false) > p.score(&worn, false));
    }

    #[test]
    fn fully_dead_segment_scores_maximal() {
        let p = GcPlanner::default();
        let dead = seg(1, 0, HOUR, 0);
        assert_eq!(p.score(&dead, false), u64::MAX / 2);
    }

    #[test]
    fn zero_freeable_segment_never_scores() {
        let p = GcPlanner::default();
        assert_eq!(p.score(&seg(1, SEG, DAY, 0), false), 0);
    }

    #[test]
    fn plan_is_capped_at_max_segments() {
        let mut cfg = GcConfig::default();
        cfg.max_segments_per_plan = 3;
        let p = GcPlanner::new(cfg);
        let stats: Vec<SegmentStat> = (0..10).map(|i| seg(i, SEG / 4, DAY, 0)).collect();
        let plan = p.plan(&stats, 100 * SEG, 10 * SEG).expect("10% free -> aggressive");
        assert_eq!(plan.segments.len(), 3);
        // Deterministic tiebreak: equal scores order by segment id.
        assert_eq!(plan.segments, vec![0, 1, 2]);
    }

    #[test]
    fn all_live_pool_plans_nothing_even_when_low_on_space() {
        // Free space below watermark but every segment 100% live: the
        // honest answer is None -- the pool is full of live data, and
        // the operator needs to hear that, not get an infinite loop.
        let p = GcPlanner::default();
        let stats = vec![seg(1, SEG, DAY, 0), seg(2, SEG, DAY, 0)];
        assert!(p.plan(&stats, 100 * SEG, 5 * SEG).is_none());
    }

    #[test]
    fn custom_watermarks_are_respected() {
        let mut cfg = GcConfig::default();
        cfg.kick_pct = 50;
        cfg.aggressive_pct = 25;
        let p = GcPlanner::new(cfg);
        let stats = vec![seg(1, SEG / 2, DAY, 0)];
        // 40% free: below custom kick (50%), above aggressive (25%).
        let plan = p.plan(&stats, 100 * SEG, 40 * SEG).expect("40% < 50%");
        assert_eq!(plan.urgency, GcUrgency::Background);
        // 20% free: below custom aggressive.
        let plan = p.plan(&stats, 100 * SEG, 20 * SEG).expect("20% < 25%");
        assert_eq!(plan.urgency, GcUrgency::Aggressive);
    }

    #[test]
    fn reclaim_event_is_a_plain_record() {
        let e = ReclaimEvent { segment_id: 3, freed_bytes: 4096, at_ns: 99 };
        assert_eq!(e, e);
    }
}
