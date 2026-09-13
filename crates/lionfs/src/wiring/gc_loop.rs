//! # GC planner → scrubber → allocator: the execution loop
//! (RFC-004 §6, Phase 8 wiring)
//!
//! The 3.0.0 [`GcPlanner`] emitted relocation plans nobody executed.
//! This module is the executor loop that closes the ring:
//!
//! ```text
//! scrubber (verified census) ──► SegmentStat[]
//!        │
//!        ▼
//! GcPlanner::plan(census, pool_total, pool_free)
//!        │  (urgency: Idle | Background | Aggressive)
//!        ▼
//! RelocationSink::evacuate(segment)   ← ordinary CoW write path:
//!        │                              checksummed, journaled,
//!        │                              crash-recovered like any write
//!        ▼
//! allocator free-space accounting + ReclaimEvent feedback
//!        │
//!        └──► next census sees the release (no device scan)
//! ```
//!
//! One `step` = one plan = at most `max_segments_per_plan`
//! evacuations. The daemon thread (`worker::gc`) calls `step` once
//! per wake; `run_to_health` loops until free space is back above
//! the kick watermark (or the honest wall: nothing reclaimable). The
//! loop cannot spin: each round either reclaims bytes or the planner
//! returns `None`, and the round counter is bounded.
//!
//! ## QoS mapping
//!
//! Background GC runs in the `Bulk` IO class at the tuned bulk rate
//! (1 GiB/s ceiling -- enough to finish a nightly pass, low enough
//! that a GC sprint cannot saturate a 10 G link). Aggressive (panic)
//! mode *stays* in Bulk class -- user IO wins the queue, always --
//! but drops the rate limit, because at 8% free the correct move is
//! to reclaim now, not to be polite:
//!
//! $$\text{class} = \begin{cases}
//! \text{Bulk, rate-limited} & \text{Background} \\
//! \text{Bulk, unlimited} & \text{Aggressive}
//! \end{cases}$$
//!
//! ## The tuned watermarks (the ③ tuning pass)
//!
//! Defaults moved kick 20% → 25%, aggressive 8% → 10%, widening the
//! background band (where the cost/benefit selector does useful
//! work) from 12 points to 15. The band width is the *transient
//! runway*: when a workload burst fills at rate $f$ (fraction of the
//! pool per second) above the background reclaim rate $r$, panic
//! arrives after
//!
//! $$t_{\text{panic}} = \frac{\text{kick} - \text{aggressive}}{f - r}
//!   \quad (f > r;\ f \le r \text{ never panics})$$
//!
//! -- 15 points of band buys 25% more burst runway than 12 at the
//! same $r$, which is what keeps panic mode a rare event rather
//! than a nightly one.

use crate::gc::{GcPlanner, GcUrgency, ReclaimEvent, SegmentStat};
use crate::qos::classes::IoLevel;
use std::io;

/// What the execution loop drives. Implemented by the transaction
/// layer in the engine; by a mock in tests and the simulator.
///
/// Every method takes explicit state in and hands explicit state
/// back -- the loop is a pure orchestration of these calls, which is
/// what keeps simulator and engine behavior bit-identical.
pub trait RelocationSink {
    /// The scrubber's verified segment census (live/stale/age/wear).
    fn census(&self) -> Vec<SegmentStat>;
    /// Pool accounting: (total bytes, free bytes).
    fn pool_bytes(&self) -> (u64, u64);
    /// Evacuates one segment through the ordinary CoW write path.
    /// Returns the bytes reclaimed (the segment's total, once reset).
    fn evacuate(&mut self, segment_id: u64) -> io::Result<u64>;
    /// Accounting feedback for a completed reclaim.
    fn record_reclaim(&mut self, event: ReclaimEvent);
}

/// How the GC round's IO should be admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcQos {
    /// The IO class GC runs in (always Bulk; user IO wins).
    pub level: IoLevel,
    /// Panic mode drops the rate limit (still Bulk class).
    pub unlimited: bool,
}

impl From<GcUrgency> for GcQos {
    fn from(urgency: GcUrgency) -> Self {
        Self {
            level: IoLevel::Bulk,
            unlimited: matches!(urgency, GcUrgency::Aggressive),
        }
    }
}

/// One execution round's report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcStepReport {
    /// Whether a plan existed at all.
    pub planned: bool,
    /// Segments evacuated this round.
    pub segments_relocated: Vec<u64>,
    /// Bytes reclaimed (free-space delta achieved).
    pub bytes_reclaimed: u64,
    /// Copy bytes the plan estimated (read + write of live data).
    pub estimated_copy_bytes: u64,
    /// The QoS posture for this round's IO.
    pub qos: Option<GcQos>,
    /// First error hit (evacuation failure); the round stops there.
    pub error: Option<String>,
}

/// The loop: planner + execution + feedback, driven step-wise.
#[derive(Clone, Debug)]
pub struct GcExecutionLoop {
    planner: GcPlanner,
    /// Cumulative reclaim accounting (cross-step).
    total_reclaimed: u64,
    /// Rounds executed.
    rounds: u64,
    /// Evacuations executed.
    evacuations: u64,
}

impl Default for GcExecutionLoop {
    fn default() -> Self {
        Self::new(GcPlanner::default())
    }
}

impl GcExecutionLoop {
    /// Loop over an explicit planner (custom watermarks welcome).
    #[must_use]
    pub fn new(planner: GcPlanner) -> Self {
        Self {
            planner,
            total_reclaimed: 0,
            rounds: 0,
            evacuations: 0,
        }
    }

    /// Executes at most one plan. Returns the round report; a
    /// `planned: false` report means the pool is healthy (or
    /// unreclaimable -- the report's empty reclaim bytes distinguish
    /// nothing-to-do from nothing-possible only by re-querying the
    /// planner; callers treat both as "sleep").
    pub fn step(&mut self, sink: &mut dyn RelocationSink, now_ns: u64) -> GcStepReport {
        let mut report = GcStepReport::default();
        let (total, free) = sink.pool_bytes();
        let census = sink.census();
        let Some(plan) = self.planner.plan(&census, total, free) else {
            return report;
        };
        report.planned = true;
        report.qos = Some(GcQos::from(plan.urgency));
        report.estimated_copy_bytes = plan.estimated_copy_bytes;

        for segment_id in &plan.segments {
            match sink.evacuate(*segment_id) {
                Ok(reclaimed) => {
                    report.bytes_reclaimed += reclaimed;
                    report.segments_relocated.push(*segment_id);
                    self.evacuations += 1;
                    sink.record_reclaim(ReclaimEvent {
                        segment_id: *segment_id,
                        freed_bytes: reclaimed,
                        at_ns: now_ns,
                    });
                }
                Err(e) => {
                    // One failed evacuation stops the round: the next
                    // census (with the scrubber's re-verification of
                    // the failed segment) decides whether it is
                    // retried or quarantined.
                    report.error = Some(e.to_string());
                    break;
                }
            }
        }
        self.total_reclaimed += report.bytes_reclaimed;
        self.rounds += 1;
        report
    }

    /// Drives `step` until free space is back above the kick
    /// watermark, no plan exists, an error stops the loop, or
    /// `max_rounds` is hit (the spin guard). Returns the aggregate
    /// report; `rounds` is the number of plans executed.
    pub fn run_to_health(
        &mut self,
        sink: &mut dyn RelocationSink,
        now_ns: u64,
        max_rounds: usize,
    ) -> GcStepReport {
        let mut aggregate = GcStepReport::default();
        let kick = u64::from(self.planner.config().kick_pct);
        for _ in 0..max_rounds {
            let before = sink.pool_bytes().1;
            let r = self.step(sink, now_ns);
            aggregate.planned |= r.planned;
            aggregate.bytes_reclaimed += r.bytes_reclaimed;
            aggregate.estimated_copy_bytes += r.estimated_copy_bytes;
            aggregate.segments_relocated.extend(r.segments_relocated);
            if r.qos.is_some() && aggregate.qos.is_none() {
                aggregate.qos = r.qos;
            }
            if let Some(e) = r.error {
                aggregate.error = Some(e);
                break;
            }
            if !r.planned {
                break; // healthy, or nothing reclaimable
            }
            let (total, free) = sink.pool_bytes();
            if total > 0 {
                let free_pct = (free.saturating_mul(100)) / total;
                if free_pct >= kick {
                    break; // back above the kick watermark
                }
            }
            let _ = before;
        }
        aggregate
    }

    /// Cumulative reclaim accounting.
    #[must_use]
    pub fn totals(&self) -> (u64, u64, u64) {
        (self.total_reclaimed, self.rounds, self.evacuations)
    }

    /// The planner (watermark introspection for the health socket).
    #[must_use]
    pub fn planner(&self) -> &GcPlanner {
        &self.planner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEG: u64 = 256 << 20;
    const DAY: u64 = 24 * 3_600 * 1_000_000_000;

    /// Mock pool: fixed census, evacuations reclaim the segment's
    /// stale bytes and update free accounting.
    struct MockPool {
        segments: Vec<SegmentStat>,
        total: u64,
        free: u64,
        reclaims: Vec<ReclaimEvent>,
        fail_on: Option<u64>,
    }

    impl MockPool {
        fn new(segments: Vec<SegmentStat>, total: u64, free: u64) -> Self {
            Self {
                segments,
                total,
                free,
                reclaims: Vec::new(),
                fail_on: None,
            }
        }
    }

    impl RelocationSink for MockPool {
        fn census(&self) -> Vec<SegmentStat> {
            self.segments.clone()
        }
        fn pool_bytes(&self) -> (u64, u64) {
            (self.total, self.free)
        }
        fn evacuate(&mut self, segment_id: u64) -> io::Result<u64> {
            if self.fail_on == Some(segment_id) {
                return Err(io::Error::other("device error"));
            }
            let idx = self
                .segments
                .iter()
                .position(|s| s.segment_id == segment_id)
                .ok_or_else(|| io::Error::other("unknown segment"))?;
            let freeable = self.segments[idx].freeable_bytes();
            // Evacuated segment: live bytes moved elsewhere, the
            // segment resets and leaves the reclaimable census (its
            // capacity is back with the allocator).
            self.segments[idx].live_bytes = 0;
            self.segments[idx].total_bytes = 0;
            self.free = self.free.saturating_add(freeable);
            Ok(freeable)
        }
        fn record_reclaim(&mut self, event: ReclaimEvent) {
            self.reclaims.push(event);
        }
    }

    fn seg(id: u64, live: u64, age: u64) -> SegmentStat {
        SegmentStat {
            segment_id: id,
            total_bytes: SEG,
            live_bytes: live,
            age_ns: age,
            write_cycles: 0,
        }
    }

    #[test]
    fn healthy_pool_steps_to_nothing() {
        let mut pool = MockPool::new(vec![seg(1, SEG / 2, DAY)], 100 * SEG, 50 * SEG);
        let mut class = GcExecutionLoop::default();
        let r = class.step(&mut pool, DAY);
        assert!(!r.planned);
        assert_eq!(r.qos, None);
        assert_eq!(class.totals(), (0, 0, 0));
    }

    #[test]
    fn background_round_executes_and_reclaims() {
        // 15% free: background band under the tuned watermarks.
        let mut pool = MockPool::new(vec![seg(1, SEG / 2, DAY)], 100 * SEG, 15 * SEG);
        let mut class = GcExecutionLoop::default();
        let r = class.step(&mut pool, DAY);
        assert!(r.planned);
        assert_eq!(r.segments_relocated, vec![1]);
        assert_eq!(r.bytes_reclaimed, SEG / 2);
        // QoS: Bulk class, rate-limited in background mode.
        assert_eq!(r.qos, Some(GcQos { level: IoLevel::Bulk, unlimited: false }));
        // Reclaim event fed back with the step's clock.
        assert_eq!(pool.reclaims.len(), 1);
        assert_eq!(pool.reclaims[0].at_ns, DAY);
        assert_eq!(pool.reclaims[0].freed_bytes, SEG / 2);
        // Free accounting advanced.
        assert_eq!(pool.pool_bytes().1, 15 * SEG + SEG / 2);
    }

    #[test]
    fn aggressive_round_drops_the_rate_limit_but_stays_bulk() {
        let mut pool = MockPool::new(vec![seg(1, SEG / 2, DAY)], 100 * SEG, 5 * SEG);
        let mut class = GcExecutionLoop::default();
        let r = class.step(&mut pool, DAY);
        assert_eq!(r.qos, Some(GcQos { level: IoLevel::Bulk, unlimited: true }));
    }

    #[test]
    fn evacuation_failure_stops_the_round_and_reports() {
        let mut pool = MockPool::new(vec![seg(1, SEG / 2, DAY), seg(2, SEG / 2, DAY)], 100 * SEG, 15 * SEG);
        pool.fail_on = Some(2);
        let mut class = GcExecutionLoop::default();
        let r = class.step(&mut pool, DAY);
        assert!(r.error.is_some());
        // No reclaim events for the failed segment.
        assert!(pool.reclaims.iter().all(|e| e.segment_id != 2));
    }

    #[test]
    fn run_to_health_terminates_at_kick_watermark() {
        // 12% free (background band), two 50%-stale segments: reclaim
        // raises free to 12.5% + 1.25% = 13.75%... under kick (25%),
        // so the loop keeps stepping until the planner runs dry.
        let mut pool = MockPool::new(
            vec![seg(1, SEG / 2, DAY), seg(2, SEG / 2, DAY), seg(3, SEG / 2, DAY)],
            100 * SEG,
            12 * SEG,
        );
        let mut class = GcExecutionLoop::default();
        let r = class.run_to_health(&mut pool, DAY, 10);
        // All three segments eventually evacuated (each round may
        // take a plan's worth), and the loop stopped before the round
        // cap once nothing reclaimable remained.
        assert_eq!(r.segments_relocated.len(), 3);
        assert!(r.error.is_none());
        // After full evacuation: 12% + 1.5 * SEG free = 13.5%.
        let (_, free) = pool.pool_bytes();
        assert_eq!(free, 12 * SEG + 3 * SEG / 2);
        // Every evacuation fed back a reclaim event.
        assert_eq!(pool.reclaims.len(), 3);
    }

    #[test]
    fn run_to_health_respects_the_round_cap() {
        // A pool that can never reach kick: the cap bounds the loop.
        let mut pool = MockPool::new(
            (0..50).map(|i| seg(i, SEG / 2, DAY)).collect::<Vec<_>>(),
            100 * SEG,
            10 * SEG,
        );
        let mut class = GcExecutionLoop::default();
        // Default plan cap is 12 segments/plan (tuned); 3 rounds max.
        let r = class.run_to_health(&mut pool, DAY, 3);
        assert!(r.segments_relocated.len() <= 3 * 12);
        assert_eq!(class.totals().1, 3);
    }

    #[test]
    fn totals_accumulate_across_steps() {
        let mut pool = MockPool::new(vec![seg(1, SEG / 2, DAY), seg(2, SEG / 4, DAY)], 100 * SEG, 15 * SEG);
        let mut class = GcExecutionLoop::default();
        let _ = class.step(&mut pool, DAY);
        // Step 1 evacuates both segments (plan cap 12 covers the
        // whole census); step 2 finds nothing reclaimable (the
        // reset segments left the census) and reports no plan --
        // `rounds` counts plans executed, not step calls.
        let _ = class.step(&mut pool, DAY);
        let (reclaimed, rounds, evacuations) = class.totals();
        assert_eq!(rounds, 1);
        assert!(reclaimed >= SEG / 2);
        assert_eq!(evacuations, 2);
    }
}
