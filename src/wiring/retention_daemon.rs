//! # Retention into the snapshot daemon; rebalance into the pool
//! manager (RFC-004 §12, Phase 8 wiring)
//!
//! Two daemons, one shape: a pure policy that 3.0.0 shipped without
//! a driver, now driven step-wise with caller-supplied time.
//!
//! * [`RetentionDaemon`] -- the snapshot daemon's retention tick. The
//!   daemon collects [`SnapshotStamp`]s from the snapshot manager,
//!   hands them to [`crate::fs::retention::apply_retention`], and
//!   expires what the GFS policy releases through the
//!   [`SnapshotDeleter`] seam (the `fs::snapshots` delete path + GC
//!   reclaim behind it). The daemon rate-limits itself: a retention
//!   pass every `min_interval_ns`, because re-evaluating an unchanged
//!   snapshot list at fsync frequency is wasted work -- the policy
//!   output only changes when snapshots are created or time passes a
//!   tier boundary.
//! * [`RebalanceDriver`] -- the pool manager's balance tick. Each
//!   `step` runs one [`RebalancePlanner::plan_round`] over the
//!   current [`DeviceCensus`] and executes the round's [`Move`]s
//!   through the [`SegmentMover`] seam (extent relocation in the
//!   Bulk class, bounded by the round byte budget). The driver loops
//!   until [`RebalancePlanner::is_balanced`] or the round cap; a
//!   census that says "balanced" costs one planner call, no IO.
//!
//! ## Why the retention tick is time-parameterized
//!
//! GFS tier boundaries are calendar functions of snapshot timestamps
//! (hour-of-day, day, ISO week, month, year). A retention pass at
//! time $t$ keeps
//!
//! $$K(t) = H(t) \cup D(t) \cup W(t) \cup M(t) \cup Y(t)$$
//!
//! where each tier set is the newest-per-bucket selection under the
//! tier budget. Two passes at the same $t$ over the same stamps
//! produce the same keep-set (the policy is pure); passes at
//! different $t$ differ only when a bucket boundary was crossed or
//! stamps changed. The daemon's interval is therefore a cache of a
//! pure function, and the deterministic simulator exercises boundary
//! crossings by advancing the clock, not by sleeping.

use crate::fs::retention::{apply_retention, RetentionPolicy, SnapshotStamp};
use crate::pool::rebalance::{DeviceCensus, Move, RebalancePlanner};
use std::io;

/// What the retention daemon drives: expiring one snapshot (the
/// `fs::snapshots` delete path, refcount drop, and the GC reclaim
/// that follows). Implemented by the engine's snapshot manager; by a
/// mock in tests and the simulator.
pub trait SnapshotDeleter {
    /// Expires snapshot `id`. `Ok(false)` = unknown id (already
    /// gone); the daemon treats it as success (idempotency).
    fn expire_snapshot(&mut self, id: u64) -> io::Result<bool>;
}

/// What the rebalance driver drives: executing one round's moves.
pub trait SegmentMover {
    /// Moves `bytes` of extents from device `from` to device `to`.
    /// Returns bytes actually moved (may be less: allocator bound).
    fn move_bytes(&mut self, m: &Move) -> io::Result<u64>;
    /// The current census (pool manager's device table).
    fn census(&self) -> Vec<DeviceCensus>;
}

/// One retention pass's report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionStepReport {
    /// Whether the pass ran (interval honored) or was skipped.
    pub ran: bool,
    /// Snapshots kept (the GFS keep-set size).
    pub kept: usize,
    /// Snapshots the daemon expired this pass.
    pub expired: Vec<u64>,
    /// Expirations that failed (device error); retried next pass.
    pub failed: Vec<u64>,
}

/// One rebalance round's report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebalanceStepReport {
    pub rounds: usize,
    pub bytes_moved: u64,
    pub moves: usize,
    pub balanced: bool,
    pub error: Option<String>,
}

/// The snapshot daemon's retention half, step-wise and deterministic.
#[derive(Clone, Debug)]
pub struct RetentionDaemon {
    policy: RetentionPolicy,
    /// Minimum interval between retention passes (ns).
    min_interval_ns: u64,
    /// Last pass time (ns); `None` = never ran.
    last_pass: Option<u64>,
    /// Passes executed.
    passes: u64,
    /// Expirations executed (cumulative).
    expired_total: u64,
}

impl Default for RetentionDaemon {
    fn default() -> Self {
        Self::new(RetentionPolicy::default(), 3_600 * 1_000_000_000) // hourly
    }
}

impl RetentionDaemon {
    /// Daemon with an explicit policy and pass interval.
    #[must_use]
    pub fn new(policy: RetentionPolicy, min_interval_ns: u64) -> Self {
        Self {
            policy,
            min_interval_ns,
            last_pass: None,
            passes: 0,
            expired_total: 0,
        }
    }

    /// One tick: evaluates retention if the interval allows (or
    /// `force` is set), expiring through the deleter. `now_ns` is
    /// caller-supplied; the same (stamps, now) always yields the
    /// same verdict.
    pub fn step(
        &mut self,
        now_ns: u64,
        snaps: &[SnapshotStamp],
        deleter: &mut dyn SnapshotDeleter,
        force: bool,
    ) -> RetentionStepReport {
        if !force {
            if let Some(last) = self.last_pass {
                if now_ns.saturating_sub(last) < self.min_interval_ns {
                    return RetentionStepReport::default(); // skipped
                }
            }
        }
        self.last_pass = Some(now_ns);
        self.passes += 1;

        let verdict = apply_retention(snaps, &self.policy);
        let mut report = RetentionStepReport {
            ran: true,
            kept: verdict.keep.len(),
            expired: Vec::new(),
            failed: Vec::new(),
        };
        for id in &verdict.expire {
            match deleter.expire_snapshot(*id) {
                Ok(_) => {
                    report.expired.push(*id);
                    self.expired_total += 1;
                }
                Err(_) => report.failed.push(*id),
            }
        }
        report
    }

    /// Cumulative accounting: (passes, expirations).
    #[must_use]
    pub fn totals(&self) -> (u64, u64) {
        (self.passes, self.expired_total)
    }

    /// The policy (health-socket introspection).
    #[must_use]
    pub fn policy(&self) -> &RetentionPolicy {
        &self.policy
    }
}

/// The pool manager's rebalance half, step-wise and deterministic.
#[derive(Clone, Debug)]
pub struct RebalanceDriver {
    planner: RebalancePlanner,
    /// Cumulative accounting.
    total_moved: u64,
    rounds: u64,
}

impl Default for RebalanceDriver {
    fn default() -> Self {
        Self::new(RebalancePlanner::default())
    }
}

impl RebalanceDriver {
    /// Driver over an explicit planner (custom round budget welcome).
    #[must_use]
    pub fn new(planner: RebalancePlanner) -> Self {
        Self {
            planner,
            total_moved: 0,
            rounds: 0,
        }
    }

    /// One round: plan against the current census, execute the
    /// moves. Returns the round report; `balanced: true` with zero
    /// moves is the steady state (one planner call, no IO).
    pub fn step(&mut self, mover: &mut dyn SegmentMover) -> RebalanceStepReport {
        let census = mover.census();
        let mut report = RebalanceStepReport {
            balanced: self.planner.is_balanced(&census),
            ..Default::default()
        };
        if report.balanced {
            return report;
        }
        let plan = self.planner.plan_round(&census);
        report.rounds = 1;
        self.rounds += 1;
        report.moves = plan.moves.len();
        for m in &plan.moves {
            match mover.move_bytes(m) {
                Ok(bytes) => {
                    report.bytes_moved += bytes;
                    self.total_moved += bytes;
                }
                Err(e) => {
                    report.error = Some(e.to_string());
                    break;
                }
            }
        }
        report
    }

    /// Drives rounds until balanced, error, or the cap. A leaving
    /// device drains in proportion to its gap; the cap keeps the
    /// daemon honest about very large drains (it will finish over
    /// several ticks, not one).
    pub fn run_to_balance(&mut self, mover: &mut dyn SegmentMover, max_rounds: usize) -> RebalanceStepReport {
        let mut aggregate = RebalanceStepReport::default();
        for _ in 0..max_rounds {
            let r = self.step(mover);
            aggregate.rounds += r.rounds;
            aggregate.bytes_moved += r.bytes_moved;
            aggregate.moves += r.moves;
            aggregate.balanced = r.balanced;
            if r.balanced || r.rounds == 0 {
                break;
            }
            if let Some(e) = r.error {
                aggregate.error = Some(e);
                break;
            }
        }
        aggregate
    }

    /// Cumulative accounting: (bytes moved, rounds).
    #[must_use]
    pub fn totals(&self) -> (u64, u64) {
        (self.total_moved, self.rounds)
    }

    /// The planner (health-socket introspection).
    #[must_use]
    pub fn planner(&self) -> &RebalancePlanner {
        &self.planner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::failure::RiskBand;

    struct MockDeleter {
        deleted: Vec<u64>,
        fail_on: Option<u64>,
    }

    impl SnapshotDeleter for MockDeleter {
        fn expire_snapshot(&mut self, id: u64) -> io::Result<bool> {
            if self.fail_on == Some(id) {
                return Err(io::Error::other("device error"));
            }
            self.deleted.push(id);
            Ok(true)
        }
    }

    struct MockPool {
        devices: Vec<DeviceCensus>,
        moves: Vec<Move>,
        fail_on: Option<u32>,
    }

    impl SegmentMover for MockPool {
        fn move_bytes(&mut self, m: &Move) -> io::Result<u64> {
            if self.fail_on == Some(m.from) {
                return Err(io::Error::other("relocation failed"));
            }
            // Apply the move to the census.
            for d in &mut self.devices {
                if d.device == m.from {
                    d.used = d.used.saturating_sub(m.bytes);
                } else if d.device == m.to {
                    d.used = d.used.saturating_add(m.bytes);
                }
            }
            self.moves.push(*m);
            Ok(m.bytes)
        }
        fn census(&self) -> Vec<DeviceCensus> {
            self.devices.clone()
        }
    }

    const HOUR: u64 = 3_600;
    const NS: u64 = 1_000_000_000;

    fn stamps(times: &[u64]) -> Vec<SnapshotStamp> {
        times
            .iter()
            .enumerate()
            .map(|(i, t)| SnapshotStamp { id: i as u64 + 1, at: *t })
            .collect()
    }

    #[test]
    fn retention_pass_expires_what_gfs_releases() {
        // 100 hourly snapshots across ~11.5 days: the hourly tier
        // keeps the tuned budget (48); daily/weekly/monthly reps add
        // keep-set members beyond it; the partition is total.
        let times: Vec<u64> = (0..100).map(|i| 1_000_000 - i as u64 * HOUR).collect();
        let snaps = stamps(&times);
        let mut daemon = RetentionDaemon::default();
        let mut deleter = MockDeleter { deleted: vec![], fail_on: None };
        let r = daemon.step(1_000_000, &snaps, &mut deleter, false);
        assert!(r.ran);
        assert!(r.kept >= 48, "kept {} (hourly floor is the tuned 48)", r.kept);
        assert_eq!(r.kept + r.expired.len(), 100); // total partition
        assert!(r.failed.is_empty());
        assert_eq!(deleter.deleted.len(), r.expired.len());
    }

    #[test]
    fn interval_skips_repeats_and_force_overrides() {
        let snaps = stamps(&[100, 200, 300]);
        let mut daemon = RetentionDaemon::default(); // 1 h interval
        let mut deleter = MockDeleter { deleted: vec![], fail_on: None };
        // First pass at t=0: runs (no interval to honor yet).
        assert!(daemon.step(0, &snaps, &mut deleter, false).ran);
        // t=1s (< 1h interval): skipped.
        assert!(!daemon.step(NS, &snaps, &mut deleter, false).ran);
        // t=1h+1s: runs again.
        assert!(daemon.step(3_600 * NS + NS, &snaps, &mut deleter, false).ran);
        // force bypasses the interval.
        assert!(daemon.step(3_600 * NS + 2 * NS, &snaps, &mut deleter, true).ran);
        let (passes, _) = daemon.totals();
        assert_eq!(passes, 3);
    }

    #[test]
    fn failed_expiration_is_reported_and_retried() {
        // 100 snapshots: the expiry set is non-empty; fail the oldest
        // (a certain expiree).
        let times: Vec<u64> = (0..100).map(|i| 1_000_000 - i as u64 * HOUR).collect();
        let snaps = stamps(&times);
        let mut daemon = RetentionDaemon::default();
        let mut deleter = MockDeleter { deleted: vec![], fail_on: Some(100) };
        let r = daemon.step(1_000_000, &snaps, &mut deleter, false);
        assert_eq!(r.failed, vec![100]);
        assert!(!r.expired.is_empty());
        // The next pass (forced) retries id 100; the failure clears.
        deleter.fail_on = None;
        let r2 = daemon.step(1_000_001, &snaps, &mut deleter, true);
        assert!(r2.failed.is_empty());
        assert!(deleter.deleted.contains(&100));
    }

    #[test]
    fn empty_stamp_list_is_a_noop() {
        let mut daemon = RetentionDaemon::default();
        let mut deleter = MockDeleter { deleted: vec![], fail_on: None };
        let r = daemon.step(0, &[], &mut deleter, false);
        assert!(r.ran);
        assert_eq!(r.kept, 0);
        assert!(r.expired.is_empty());
    }

    #[test]
    fn balanced_pool_costs_one_planner_call_no_io() {
        let pool = MockPool {
            devices: vec![
                DeviceCensus { device: 1, capacity: 100, used: 50, health: RiskBand::Healthy, leaving: false },
                DeviceCensus { device: 2, capacity: 100, used: 50, health: RiskBand::Healthy, leaving: false },
            ],
            moves: vec![],
            fail_on: None,
        };
        let mut driver = RebalanceDriver::default();
        let mut pool = pool;
        let r = driver.step(&mut pool);
        assert!(r.balanced);
        assert_eq!(r.moves, 0);
        assert_eq!(r.bytes_moved, 0);
    }

    #[test]
    fn lopsided_pool_rebalances_until_balanced() {
        let mut pool = MockPool {
            devices: vec![
                DeviceCensus { device: 1, capacity: 100, used: 90, health: RiskBand::Healthy, leaving: false },
                DeviceCensus { device: 2, capacity: 100, used: 10, health: RiskBand::Healthy, leaving: false },
            ],
            moves: vec![],
            fail_on: None,
        };
        let mut driver = RebalanceDriver::default();
        let r = driver.run_to_balance(&mut pool, 20);
        // 40 bytes must move (90-50 -> target 50).
        assert!(r.bytes_moved >= 40, "moved {} bytes", r.bytes_moved);
        assert!(r.error.is_none());
        // Converged: a fresh step reports balanced with no moves.
        let r2 = driver.step(&mut pool);
        assert!(r2.balanced);
        assert_eq!(r2.moves, 0);
    }

    #[test]
    fn move_failure_stops_the_round() {
        let mut pool = MockPool {
            devices: vec![
                DeviceCensus { device: 1, capacity: 100, used: 90, health: RiskBand::Healthy, leaving: false },
                DeviceCensus { device: 2, capacity: 100, used: 10, health: RiskBand::Healthy, leaving: false },
            ],
            moves: vec![],
            fail_on: Some(1),
        };
        let mut driver = RebalanceDriver::default();
        let r = driver.run_to_balance(&mut pool, 5);
        assert!(r.error.is_some());
    }

    #[test]
    fn leaving_device_drains_absolutely_first() {
        let mut pool = MockPool {
            devices: vec![
                DeviceCensus { device: 1, capacity: 100, used: 30, health: RiskBand::Healthy, leaving: true },
                DeviceCensus { device: 2, capacity: 100, used: 70, health: RiskBand::Healthy, leaving: false },
                DeviceCensus { device: 3, capacity: 100, used: 10, health: RiskBand::Healthy, leaving: false },
            ],
            moves: vec![],
            fail_on: None,
        };
        let mut driver = RebalanceDriver::default();
        let r = driver.run_to_balance(&mut pool, 30);
        // The leaving device fully drained.
        assert_eq!(pool.census().iter().find(|d| d.device == 1).unwrap().used, 0);
        assert!(r.bytes_moved >= 30);
    }
}
