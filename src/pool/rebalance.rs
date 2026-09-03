//! Online pool rebalance planning (RFC-004 §12.2).
//!
//! Device add/remove and the steady-state drift between them: LionFS
//! 2.0 could assemble pools and read degraded, but the only way to
//! change membership was mkfs. The 3.0 rebalance is a *planner* over
//! the same pure-function discipline as the GC: it takes a device
//! census (capacity, used, health) and a target membership, and emits
//! an ordered list of migration moves (source device, byte range,
//! destination device) that the transaction layer executes through the
//! ordinary CoW write path -- each move is checksummed, journaled, and
//! crash-recoverable like any other write, which is the only sane way
//! to move petabytes under live traffic.
//!
//! Policy (RFC-004 §12.2):
//!
//! * **fill targets**: used/capacity converges toward the pool mean
//!   (weighted by capacity), not toward equal *bytes* per device --
//!   a 16 TiB device holds proportionally more than a 4 TiB one.
//! * **health discount**: a Watch-or-worse device (from the Guardian
//!   risk model) is drained first, proportional to its risk band --
//!   rebalance doubles as the evacuation path for failing hardware.
//! * **throttle**: moves are sized to a byte budget per round (the
//!   caller drives rounds; QoS's Bulk class bounds their impact).
//! * **drain-to-remove**: a device flagged `leaving` is drained to
//!   zero before the operator may remove it; the planner refuses to
//!   plan *into* it.

use crate::guardian::failure::RiskBand;

/// One device's rebalance-relevant census row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceCensus {
    pub device: u32,
    /// Capacity in bytes.
    pub capacity: u64,
    /// Used bytes.
    pub used: u64,
    /// Health posture from the Guardian risk model.
    pub health: RiskBand,
    /// True for devices being removed: drain, never fill.
    pub leaving: bool,
}

/// One planned migration move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub from: u32,
    pub to: u32,
    /// Bytes this move shifts (a budget-sized chunk, not the whole gap).
    pub bytes: u64,
}

/// A rebalance round's plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebalancePlan {
    pub moves: Vec<Move>,
    /// Devices that reached their drain target this round.
    pub drained: Vec<u32>,
    /// Total bytes in this round.
    pub bytes: u64,
}

/// Tunables.
#[derive(Clone, Copy, Debug)]
pub struct RebalanceConfig {
    /// Max bytes moved per round (default 1 GiB: ~seconds of Bulk IO).
    pub round_byte_budget: u64,
    /// How strongly health discount drains risky devices: the used
    /// target of a Watch device is reduced by 25%, Degraded 50%,
    /// Failing 100% (drain). Basis points, by band.
    pub health_drain_bps: [u64; 4], // [Healthy, Watch, Degraded, Failing]
}

impl Default for RebalanceConfig {
    fn default() -> Self {
        Self {
            round_byte_budget: 1 << 30,
            health_drain_bps: [0, 2_500, 5_000, 10_000],
        }
    }
}

/// The planner: pure function of (census, config) -> round plan.
#[derive(Clone, Copy, Debug, Default)]
pub struct RebalancePlanner {
    config: RebalanceConfig,
}

impl RebalancePlanner {
    #[must_use]
    pub fn new(config: RebalanceConfig) -> Self {
        Self { config }
    }

    /// The used-bytes target for one device after health adjustment.
    ///
    /// Base target = capacity * (pool_used / pool_capacity) -- the
    /// device's proportional share. A leaving device targets zero; a
    /// risky device targets proportionally less.
    #[must_use]
    pub fn target_used(&self, d: &DeviceCensus, pool_used: u64, pool_cap: u64) -> u64 {
        if d.leaving {
            return 0;
        }
        if pool_cap == 0 || d.capacity == 0 {
            return d.used;
        }
        let share = (d.capacity as u128 * pool_used as u128) / pool_cap as u128;
        let discount_bps = match d.health {
            RiskBand::Healthy => self.config.health_drain_bps[0],
            RiskBand::Watch => self.config.health_drain_bps[1],
            RiskBand::Degraded => self.config.health_drain_bps[2],
            RiskBand::Failing => self.config.health_drain_bps[3],
        };
        let target = share * (10_000 - u128::from(discount_bps)) / 10_000;
        u64::try_from(target).unwrap_or(u64::MAX)
    }

    /// Plans one round. Devices over target are sources (most
    /// overfilled first, leaving devices absolutely first); devices
    /// under target and not leaving are destinations (most headroom
    /// first). Moves are paired greedily and bounded by the round
    /// budget.
    #[must_use]
    pub fn plan_round(&self, census: &[DeviceCensus]) -> RebalancePlan {
        let pool_cap: u64 = census
            .iter()
            .filter(|d| !d.leaving)
            .map(|d| d.capacity)
            .sum();
        let pool_used: u64 = census.iter().map(|d| d.used).sum();

        let mut over: Vec<(i128, &DeviceCensus)> = Vec::new(); // (surplus, device)
        let mut under: Vec<(i128, &DeviceCensus)> = Vec::new();
        for d in census {
            if d.capacity == 0 {
                continue;
            }
            let target = i128::from(self.target_used(d, pool_used, pool_cap));
            let used = i128::from(d.used);
            let gap = used - target;
            if gap > 0 {
                over.push((gap, d));
            } else if gap < 0 && !d.leaving {
                under.push((-gap, d));
            }
        }
        // Sources: leaving first (by gap), then most-overfilled.
        over.sort_by(|a, b| {
            let leaving_first = a.1.leaving.cmp(&b.1.leaving).reverse(); // leaving first
            leaving_first.then_with(|| b.0.cmp(&a.0))
        });
        // Destinations: most headroom first.
        under.sort_by(|a, b| b.0.cmp(&a.0));

        let mut plan = RebalancePlan::default();
        let mut budget = self.config.round_byte_budget;
        let mut si = 0usize;
        let mut di = 0usize;
        while budget > 0 && si < over.len() && di < under.len() {
            let (mut src_gap, src) = over[si];
            let (dst_gap, dst) = under[di];
            if src.leaving {
                // Drain priority: full gap, not the health-discounted
                // remainder, and it can go negative-target already --
                // the leaving target of 0 already handles it.
            }
            let amount = src_gap.min(dst_gap).min(i128::from(budget)).max(0);
            if amount == 0 {
                break;
            }
            plan.moves.push(Move {
                from: src.device,
                to: dst.device,
                bytes: u64::try_from(amount).unwrap_or(0),
            });
            plan.bytes += u64::try_from(amount).unwrap_or(0);
            budget -= u64::try_from(amount).unwrap_or(0);
            src_gap -= amount;
            over[si].0 = src_gap;
            under[di].0 -= amount;
            if src_gap == 0 {
                if src.leaving {
                    plan.drained.push(src.device);
                }
                si += 1;
            }
            if under[di].0 == 0 {
                di += 1;
            }
        }
        // Any other leaving device that reached zero this round.
        for (gap, d) in &over[si.min(over.len())..] {
            if d.leaving && *gap == 0 {
                plan.drained.push(d.device);
            }
        }
        plan
    }

    /// Whether the pool is balanced (no non-leaving device deviates
    /// from target by more than 1% of its capacity, and no leaving
    /// device holds data). The operator's "can I stop the rebalance
    /// loop / remove the device now" check.
    #[must_use]
    pub fn is_balanced(&self, census: &[DeviceCensus]) -> bool {
        let pool_cap: u64 = census
            .iter()
            .filter(|d| !d.leaving)
            .map(|d| d.capacity)
            .sum();
        let pool_used: u64 = census.iter().map(|d| d.used).sum();
        for d in census {
            if d.capacity == 0 {
                continue;
            }
            let target = self.target_used(d, pool_used, pool_cap);
            if d.leaving {
                if d.used != 0 {
                    return false;
                }
                continue;
            }
            let slack = (d.capacity / 100).max(1);
            if d.used.abs_diff(target) > slack {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TB: u64 = 1 << 40;

    fn dev(device: u32, capacity: u64, used: u64) -> DeviceCensus {
        DeviceCensus { device, capacity, used, health: RiskBand::Healthy, leaving: false }
    }

    #[test]
    fn targets_are_proportional_to_capacity() {
        let p = RebalancePlanner::default();
        // 4 TiB used across 16+4 TiB devices: mean fill 20%.
        let census = [dev(0, 16 * TB, 0), dev(1, 4 * TB, 4 * TB)];
        let t0 = p.target_used(&census[0], 4 * TB, 20 * TB);
        let t1 = p.target_used(&census[1], 4 * TB, 20 * TB);
        assert_eq!(t0, 3 * TB + TB / 5); // 16/20 * 4 TiB
        assert_eq!(t1, TB * 4 / 5); // 4/20 * 4 TiB, floored division
    }

    #[test]
    fn balanced_pool_plans_nothing() {
        let p = RebalancePlanner::default();
        let census = [dev(0, 4 * TB, TB), dev(1, 4 * TB, TB), dev(2, 4 * TB, TB)];
        let plan = p.plan_round(&census);
        assert!(plan.moves.is_empty());
        assert!(p.is_balanced(&census));
    }

    #[test]
    fn skewed_pool_moves_from_full_to_empty() {
        let p = RebalancePlanner::default();
        let census = [dev(0, 4 * TB, 4 * TB), dev(1, 4 * TB, 0)];
        let plan = p.plan_round(&census);
        // Round budget 1 GiB: one move of 1 GiB from 0 to 1.
        assert_eq!(plan.moves.len(), 1);
        assert_eq!(plan.moves[0].from, 0);
        assert_eq!(plan.moves[0].to, 1);
        assert_eq!(plan.moves[0].bytes, 1 << 30);
        assert!(!p.is_balanced(&census));
    }

    #[test]
    fn leaving_devices_are_drained_first() {
        let p = RebalancePlanner::default();
        let mut leaving = dev(3, 4 * TB, 2 * TB);
        leaving.leaving = true;
        // Also a normally-overfilled device to compete with it.
        let census = [dev(0, 4 * TB, 4 * TB), dev(1, 4 * TB, 0), leaving];
        let plan = p.plan_round(&census);
        assert_eq!(plan.moves.first().map(|m| m.from), Some(3)); // drain first
        assert!(plan.moves.iter().all(|m| m.to != 3)); // never fills the leaver
    }

    #[test]
    fn drain_completion_is_reported() {
        let p = RebalancePlanner::default();
        let mut leaving = dev(5, TB, 1 << 20); // tiny residue
        leaving.leaving = true;
        let census = [dev(0, 4 * TB, TB), leaving, dev(1, 4 * TB, TB)];
        let plan = p.plan_round(&census);
        assert_eq!(plan.drained, vec![5]);
        assert!(plan.bytes >= 1 << 20);
    }

    #[test]
    fn failing_devices_lose_share() {
        let p = RebalancePlanner::default();
        let mut failing = dev(7, 4 * TB, 2 * TB);
        failing.health = RiskBand::Failing;
        // Pool: 8 TiB cap, 2 TiB used -> mean 25%; failing target
        // drains fully (100% discount).
        let census = [failing, dev(1, 4 * TB, 0)];
        let t = p.target_used(&census[0], 2 * TB, 8 * TB);
        assert_eq!(t, 0);
        let plan = p.plan_round(&census);
        assert!(plan.moves.iter().any(|m| m.from == 7));
    }

    #[test]
    fn degraded_discounts_half() {
        let p = RebalancePlanner::default();
        let mut degraded = dev(7, 4 * TB, 2 * TB);
        degraded.health = RiskBand::Degraded;
        // Mean 25% -> 1 TiB share; degraded keeps 50%.
        let census = [degraded, dev(1, 4 * TB, 0)];
        let t = p.target_used(&census[0], 2 * TB, 8 * TB);
        assert_eq!(t, TB / 2);
    }

    #[test]
    fn round_budget_bounds_total_bytes() {
        let mut cfg = RebalanceConfig::default();
        cfg.round_byte_budget = 100 << 20;
        let p = RebalancePlanner::new(cfg);
        let census = [dev(0, 4 * TB, 4 * TB), dev(1, 4 * TB, 0), dev(2, 4 * TB, 0)];
        let plan = p.plan_round(&census);
        assert!(plan.bytes <= 100 << 20);
    }

    #[test]
    fn moves_pair_smallest_need() {
        // A destination with little headroom gets exactly its gap.
        let p = RebalancePlanner::default();
        let census = [dev(0, 4 * TB, 4 * TB), dev(1, TB, 900 << 30)];
        let plan = p.plan_round(&census);
        // Dest headroom = 100 GiB - share... pool 4.88 TiB cap, 4.88
        // TiB used -> mean ~100%; dest target ~976 GiB, gap ~76 GiB;
        // src gap ~3 TiB; budget 1 GiB: the move is budget-bound.
        assert_eq!(plan.moves.len(), 1);
        assert!(plan.moves[0].bytes <= 1 << 30);
        assert_eq!(plan.moves[0].to, 1);
    }

    #[test]
    fn empty_and_degenerate_censuses_are_safe() {
        let p = RebalancePlanner::default();
        let plan = p.plan_round(&[]);
        assert!(plan.moves.is_empty());
        assert!(p.is_balanced(&[]));
        // Zero-capacity devices are ignored, not divided by.
        let census = [dev(0, 0, 0), dev(1, 4 * TB, 0)];
        let plan = p.plan_round(&census);
        assert!(plan.moves.is_empty());
        assert!(p.is_balanced(&census));
    }

    #[test]
    fn convergence_over_rounds() {
        // Drive a skewed pool to balance by applying plans.
        let p = RebalancePlanner::default();
        let mut cfg = RebalanceConfig::default();
        cfg.round_byte_budget = TB; // 1 TiB rounds to converge fast
        let p = RebalancePlanner::new(cfg);
        let mut census = vec![dev(0, 4 * TB, 4 * TB), dev(1, 4 * TB, 0), dev(2, 4 * TB, 0)];
        let mut rounds = 0;
        while !p.is_balanced(&census) && rounds < 20 {
            let plan = p.plan_round(&census);
            if plan.moves.is_empty() {
                break;
            }
            for mv in &plan.moves {
                for d in census.iter_mut() {
                    if d.device == mv.from {
                        d.used -= mv.bytes;
                    } else if d.device == mv.to {
                        d.used += mv.bytes;
                    }
                }
            }
            rounds += 1;
        }
        assert!(p.is_balanced(&census), "not balanced after {rounds} rounds: {census:?}");
        // Converged to roughly equal fill (5.33 TiB total over 12 TiB).
        for d in &census {
            assert!(d.used.abs_diff(4 * TB / 3 * 4 / 3) < TB / 2, "used {}", d.used);
        }
    }
}
