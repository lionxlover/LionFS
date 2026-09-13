//! Adaptive tiering (spec §12) — the "online learner" that decides where
//! each extent-group lives.
//!
//! Spec §12.2 defines a linear placement score:
//!
//! ```text
//! Score(e) = w1·Recency + w2·Frequency − w3·SizeCost − w4·Energy(tier) − w5·$(tier)
//! Recency   = exp(−λ·(now − last_access))
//! Frequency = log(1 + accesses) / log(1 + max_accesses)
//! ```
//!
//! with weights updated by SGD on observed latency-cost regret. The audit's
//! F-series notes this scores *extents*, not (extent, tier) pairs — the
//! feature vector underdetermines the tier choice — so this implementation
//! scores **(extent, tier) pairs** (tier-conditional features: latency,
//! energy, $, plus retrieval/egress penalty), which is the minimal fix.
//! Hysteresis is added to stop tier thrashing (audit G7): a move only fires
//! when the score gap exceeds a margin for two consecutive epochs.

use crate::core::Tier;
use crate::reliability::{zipf_cache_for_target, zipf_hit_ratio};
use serde::{Deserialize, Serialize};

/// Tuning knobs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TieringConfig {
    /// Recency decay rate λ (per second).
    pub lambda: f64,
    /// Learning rate for SGD.
    pub eta: f64,
    /// Hysteresis margin: challenger must beat incumbent by this much.
    pub hysteresis: f64,
    /// Epochs of sustained superiority required before migrating.
    pub epochs_required: u32,
}

impl Default for TieringConfig {
    fn default() -> Self {
        Self { lambda: 1.0 / (24.0 * 3600.0), eta: 0.01, hysteresis: 0.05, epochs_required: 2 }
    }
}

/// Per-extent access statistics maintained by the storage engine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtentStats {
    pub last_access_secs: f64,
    pub access_count: u64,
    pub size_bytes: u64,
}

/// The tier-placement scorer with online weight updates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlacementScorer {
    /// Weights: [recency, frequency, latency, energy, dollars, egress].
    weights: [f64; 6],
    config: TieringConfig,
    /// Max access count seen (for frequency normalization).
    max_accesses: u64,
}

impl PlacementScorer {
    pub fn new(config: TieringConfig) -> Self {
        // Initial weights bias toward latency (hot data stays fast).
        Self {
            weights: [1.0, 1.0, -0.05, -0.02, -0.01, -0.03],
            config,
            max_accesses: 1,
        }
    }

    pub fn weights(&self) -> &[f64; 6] {
        &self.weights
    }

    /// Feature vector for (extent, tier): spec §12.2's extent features plus
    /// tier-conditional cost features.
    ///
    /// Audit refinement (F-series + G7): the spec's Score(e) is *linear* in
    /// tier latency and energy with no interaction term — under the spec's
    /// own feature magnitudes, the latency term (0.008 ms … 5 min) dwarfs
    /// every dollar/energy term (10^-2 … 10^-4), so the model as written can
    /// never demote cold data to cheap tiers. The minimal fix: weight
    /// latency and energy by **hotness** (`recency · frequency`) — access
    /// costs only apply when the data is actually accessed — leaving
    /// dollars and egress as always-on terms.
    fn features(&self, extent: &ExtentStats, tier: Tier, now_secs: f64, max_accesses: u64) -> [f64; 6] {
        let age = (now_secs - extent.last_access_secs).max(0.0);
        let recency = (-self.config.lambda * age).exp();
        let frequency = ((1.0 + extent.access_count as f64).ln())
            / ((1.0 + max_accesses as f64).ln());
        let hotness = recency * frequency;
        let size_gb = extent.size_bytes as f64 / 1.0e9;
        let latency = hotness * tier.latency_ns() as f64 / 1.0e6; // ms, accessed
        let energy = hotness * tier.energy_j_per_gb() * size_gb;
        let dollars = tier.dollars_per_gb_month() * size_gb;
        // Egress asymmetry (audit G7): leaving a cold tier costs real money.
        let egress = match tier {
            Tier::Cloud | Tier::Tape => 0.02 * size_gb,
            _ => 0.0,
        };
        [recency, frequency, latency, energy, dollars, egress]
    }

    /// Score placing `extent` on `tier` — higher is better.
    pub fn score(&self, extent: &ExtentStats, tier: Tier, now_secs: f64) -> f64 {
        let f = self.features(extent, tier, now_secs, self.max_accesses);
        self.weights
            .iter()
            .zip(f.iter())
            .map(|(w, x)| w * x)
            .sum::<f64>()
    }

    /// Record an observation and take one SGD step (spec §12.2):
    /// `w ← w − η·∇Loss`, `Loss = ObservedCost − PredictedCost`.
    ///
    /// `observed_cost` is the realized latency cost (ms) of the request
    /// that just happened on `tier`; the gradient is `-features` scaled by
    /// the prediction error.
    pub fn observe(
        &mut self,
        extent: &ExtentStats,
        tier: Tier,
        now_secs: f64,
        observed_cost_ms: f64,
    ) {
        let f = self.features(extent, tier, now_secs, self.max_accesses);
        let predicted: f64 = self.weights.iter().zip(f.iter()).map(|(w, x)| w * x).sum();
        // We predict *negative* cost (score); loss = observed + predicted.
        let error = observed_cost_ms + predicted; // if predicted = -cost, loss ≈ 0
        for (w, x) in self.weights.iter_mut().zip(f.iter()) {
            *w -= self.config.eta * error * x;
        }
        self.max_accesses = self.max_accesses.max(extent.access_count).max(1);
        // Keep weights bounded (online-learner hygiene).
        for w in self.weights.iter_mut() {
            *w = w.clamp(-50.0, 50.0);
        }
    }

    /// Decide the target tier for an extent, with hysteresis.
    ///
    /// `current_tier` + `epoch` (monotone counter) + internal streak state
    /// decide whether a migration actually fires this epoch.
    pub fn choose_tier(
        &mut self,
        extent: &ExtentStats,
        current: Tier,
        now_secs: f64,
        streak: &mut TierStreak,
        epoch: u32,
    ) -> Tier {
        let ladder = [Tier::Nvme, Tier::Ssd, Tier::Hdd, Tier::Cloud, Tier::Tape];
        let mut best = current;
        let mut best_score = self.score(extent, current, now_secs);
        for &t in &ladder {
            if t == current {
                continue;
            }
            let s = self.score(extent, t, now_secs);
            if s > best_score {
                best_score = s;
                best = t;
            }
        }
        // Hysteresis: require `hysteresis` margin and sustained epochs.
        let current_score = self.score(extent, current, now_secs);
        if best != current && best_score - current_score > self.config.hysteresis {
            if streak.candidate == Some(best) {
                streak.streak += 1;
            } else {
                streak.candidate = Some(best);
                streak.streak = 1;
            }
            streak.epoch = epoch;
            if streak.streak >= self.config.epochs_required {
                streak.streak = 0;
                streak.candidate = None;
                return best;
            }
            return current;
        }
        streak.streak = 0;
        streak.candidate = None;
        current
    }
}

/// Streak state for one extent's tier decision (hysteresis bookkeeping).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TierStreak {
    pub candidate: Option<Tier>,
    pub streak: u32,
    pub epoch: u32,
}

/// Cache-tier auto-sizing (spec §12.3): given measured Zipf skew `s` over
/// `n_objects`, return the object count needed for `target` hit ratio.
///
/// This is the *object-count* model; a byte-weighted refinement is the
/// audit's G-item (extents vary in size).
pub fn size_cache_tier(n_objects: f64, s: f64, target: f64) -> f64 {
    zipf_cache_for_target(n_objects, s, target)
}

/// Convenience: hit ratio achieved by a given cache size.
pub fn hit_ratio(c_objects: f64, n_objects: f64, s: f64) -> f64 {
    zipf_hit_ratio(c_objects, n_objects, s)
}

/// Re-export the estimator so engines can feed measured access logs.
pub use crate::reliability::estimate_zipf_skew as estimate_skew;

#[cfg(test)]
mod tests {
    use super::*;

    fn hot(last: f64, accesses: u64) -> ExtentStats {
        ExtentStats { last_access_secs: last, access_count: accesses, size_bytes: 4096 }
    }

    #[test]
    fn hot_data_scores_higher_on_fast_tiers() {
        let scorer = PlacementScorer::new(TieringConfig::default());
        let extent = hot(0.0, 1000); // touched just now, very frequently
        let nvme = scorer.score(&extent, Tier::Nvme, 0.0);
        let tape = scorer.score(&extent, Tier::Tape, 0.0);
        assert!(nvme > tape, "nvme={nvme} tape={tape}");
    }

    #[test]
    fn cold_data_prefers_cheap_tiers() {
        let scorer = PlacementScorer::new(TieringConfig::default());
        let extent = ExtentStats {
            last_access_secs: 365.0 * 86400.0, // untouched for a year
            access_count: 1,
            size_bytes: 64 * 1024 * 1024, // 64 MiB
        };
        // "Now" is 30 days AFTER the last access → recency ≈ e^-30 ≈ 0.
        let now = 365.0 * 86400.0 + 30.0 * 86400.0;
        let tape = scorer.score(&extent, Tier::Tape, now);
        let nvme = scorer.score(&extent, Tier::Nvme, now);
        // Recency ≈ 0, frequency ≈ 0 → hotness ≈ 0 → latency/energy terms
        // vanish → only dollars matter → tape wins.
        assert!(tape > nvme, "tape={tape} nvme={nvme}");
    }

    #[test]
    fn choose_tier_moves_hot_data_up_with_hysteresis() {
        let mut scorer = PlacementScorer::new(TieringConfig::default());
        let extent = hot(0.0, 500);
        let mut streak = TierStreak::default();

        // First epoch: wants NVMe but hysteresis holds it on HDD.
        let t1 = scorer.choose_tier(&extent, Tier::Hdd, 0.0, &mut streak, 1);
        assert_eq!(t1, Tier::Hdd, "first epoch must not move (hysteresis)");
        // Sustained superiority → moves on the second epoch.
        let t2 = scorer.choose_tier(&extent, Tier::Hdd, 0.0, &mut streak, 2);
        assert_eq!(t2, Tier::Nvme, "second epoch should migrate");
    }

    #[test]
    fn no_thrashing_when_scores_are_close() {
        let mut scorer = PlacementScorer::new(TieringConfig::default());
        let extent = ExtentStats { last_access_secs: 0.0, access_count: 5, size_bytes: 4096 };
        let mut streak = TierStreak::default();
        // Lukewarm data: scores are close → stay put across many epochs.
        for epoch in 1..=10 {
            let t = scorer.choose_tier(&extent, Tier::Ssd, 0.0, &mut streak, epoch);
            assert_eq!(t, Tier::Ssd, "lukewarm data must not thrash (epoch {epoch})");
        }
    }

    #[test]
    fn sgd_learns_that_slow_tier_was_bad() {
        let mut scorer = PlacementScorer::new(TieringConfig {
            eta: 0.05,
            ..TieringConfig::default()
        });
        let extent = hot(0.0, 100);
        // Observed 8000 ms on tape — terrible. The learner should raise the
        // penalty (more negative weight) on tape-like latency features.
        let w_before = scorer.weights()[2];
        for _ in 0..50 {
            scorer.observe(&extent, Tier::Tape, 0.0, 8000.0);
        }
        let w_after = scorer.weights()[2];
        assert!(
            w_after < w_before,
            "latency weight should become more negative: {w_before} → {w_after}"
        );
        // And tape should now score below NVMe for this extent.
        assert!(scorer.score(&extent, Tier::Nvme, 0.0) > scorer.score(&extent, Tier::Tape, 0.0));
    }

    #[test]
    fn cache_sizing_solves_sla() {
        // Mild skew (s = 0.9): 95% hits needs ~60% of the corpus cached —
        // Zipf with s < 1 has a heavy effective tail.
        let c = size_cache_tier(1.0e7, 0.9, 0.95);
        assert!(hit_ratio(c, 1.0e7, 0.9) >= 0.95);
        assert!(c < 0.7 * 1.0e7);

        // Strong skew (s = 0.99): 95% hits needs < 1% of the corpus —
        // THIS is the regime where the Zipf cache model pays off.
        let c2 = size_cache_tier(1.0e7, 0.99, 0.95);
        assert!(hit_ratio(c2, 1.0e7, 0.99) >= 0.95);
        assert!(c2 < 1.0e5, "c2 = {c2}");
    }

    #[test]
    fn skew_estimation_end_to_end() {
        let s = 0.85;
        let counts: Vec<u64> = (1..=1000u32).map(|r| (1.0e6 / (r as f64).powf(s)) as u64).collect();
        let est = estimate_skew(&counts);
        assert!((est - s).abs() < 0.05, "est={est}");
    }
}
