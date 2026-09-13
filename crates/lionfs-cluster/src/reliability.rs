//! Reliability and performance models (spec §8.1, §12.3, §13), implemented
//! **side by side with their corrected forms** where the audit found errors.
//!
//! Every function is documented with (a) the spec's formula, (b) what the
//! literature actually supports, and (c) which one this function computes.
//! The [`hfs-cli`] `reliability` subcommand prints the comparison table so
//! the numbers can be reproduced mechanically.

use serde::Serialize;

// ---------------------------------------------------------------------------
// Write amplification (spec §8.1 / audit M1)
// ---------------------------------------------------------------------------

/// Write amplification of a log-structured cleaner at live fraction `u`,
/// assuming uniformly-random overwrite and greedy/age-hybrid cleaning.
///
/// * Spec formula: `WA = 2/(1-u)` — internally inconsistent with the spec's
///   own claim of "WA ~ 2× at 50% utilization" (2/(1-0.5) = 4).
/// * Corrected (classic LFS steady-state): to free one unit, clean `1/(1-u)`
///   units of gross space, rewriting `u/(1-u)`, so total device writes per
///   logical write = `1/(1-u)`; at u = 0.5 that is exactly 2×.
pub fn wa_lfs(u: f64) -> f64 {
    assert!((0.0..1.0).contains(&u), "utilization must be in (0,1)");
    1.0 / (1.0 - u)
}

/// The spec's (erroneous) formula, kept for comparison/audit tooling.
pub fn wa_spec(u: f64) -> f64 {
    assert!((0.0..1.0).contains(&u), "utilization must be in (0,1)");
    2.0 / (1.0 - u)
}

/// Total device write amplification when a journal (WAL) writes every
/// payload byte once before the data zone does (spec §6 step 9):
/// `1 (journal) + 1/(1-u) (data + cleaning)` — the audit's C1 accounting.
pub fn wa_with_data_journal(u: f64) -> f64 {
    1.0 + wa_lfs(u)
}

// ---------------------------------------------------------------------------
// Segment cleaner cost model (spec §8.1 / audit M2)
// ---------------------------------------------------------------------------

/// Segment info for cleaner ranking.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct SegmentInfo {
    /// Live fraction `u` ∈ [0,1].
    pub live_fraction: f64,
    /// Seconds since the segment was last written (age).
    pub age_secs: f64,
}

impl SegmentInfo {
    /// Classic Rosenblum-Ousterhout cost-benefit:
    /// `benefit/cost = ((1-u) · age) / (1+u)` — clean segments that are old
    /// and mostly dead. The spec's §8.1 text matches this, but its separate
    /// "Cost(seg) = (1-u)/age" line is the inverse *of the wrong thing*
    /// (it prefers young, mostly-free segments — the opposite of the
    /// cost-benefit intent; audit M2).
    pub fn benefit_cost(&self) -> f64 {
        ((1.0 - self.live_fraction) * self.age_secs) / (1.0 + self.live_fraction)
    }

    /// The spec's (inverted) cost formula, kept for audit tooling.
    pub fn cost_spec(&self) -> f64 {
        (1.0 - self.live_fraction) / self.age_secs.max(f64::EPSILON)
    }
}

/// Rank segments for cleaning: highest benefit/cost first (greedy + age).
pub fn rank_for_cleaning(segments: &[SegmentInfo]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..segments.len()).collect();
    idx.sort_by(|&a, &b| {
        segments[b]
            .benefit_cost()
            .partial_cmp(&segments[a].benefit_cost())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx
}

// ---------------------------------------------------------------------------
// MTTDL (spec §13.1 / audit M4)
// ---------------------------------------------------------------------------

/// Inputs for MTTDL models.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct EcReliability {
    /// Data shards per stripe.
    pub k: u32,
    /// Parity shards per stripe (tolerates any m failures).
    pub m: u32,
    /// Per-device mean time to failure, hours.
    pub device_mttf_hours: f64,
    /// Mean time to repair a failed device, hours.
    pub repair_mttr_hours: f64,
}

impl Default for EcReliability {
    fn default() -> Self {
        // Spec §13.1 example: k=8, m=3, F=10^6 h, R=4 h.
        Self {
            k: 8,
            m: 3,
            device_mttf_hours: 1.0e6,
            repair_mttr_hours: 4.0,
        }
    }
}

impl EcReliability {
    /// Number of devices in a stripe group.
    pub fn n(&self) -> u32 {
        self.k + self.m
    }

    /// MTTDL under the spec's formula:
    /// `F^(m+1) / (n · C(n-1, m) · R^m · (m+1))`.
    ///
    /// This is a rare-event approximation whose constant factor depends on
    /// unstated assumptions (exponential vs deterministic repair). It is
    /// *pessimistic by ~2×* versus the exact CTMC result for m=1 (audit M4):
    /// the exact Markov chain gives `F² / (n(n-1)R)` for RAID-5-like groups,
    /// while this formula yields `F² / (2·n(n-1)·R)`.
    pub fn mttdl_spec(&self) -> f64 {
        let n = self.n() as u64;
        let m = self.m as u64;
        let f = self.device_mttf_hours;
        let r = self.repair_mttr_hours;
        let c = binomial(n - 1, m);
        f.powi(m as i32 + 1) / ((n as f64) * (c as f64) * r.powi(m as i32) * (m as f64 + 1.0))
    }

    /// Corrected rare-event MTTDL, matching the CTMC derivation:
    /// data loss requires m+1 devices to fail within overlapping repair
    /// windows; first failure at rate nλ, then the probability that m more
    /// devices fail before repair completes is ∏(n-i)·λR for i=1..m:
    ///
    /// `MTTDL ≈ F^(m+1) / (n · m! · C(n-1, m) · R^m)`
    ///
    /// For m=1 this reproduces the exact CTMC result `F²/(n(n-1)R)`
    /// (n·C(n-1,1) = n(n-1)).
    pub fn mttdl_ctmc(&self) -> f64 {
        let n = self.n() as u64;
        let m = self.m as u64;
        let f = self.device_mttf_hours;
        let r = self.repair_mttr_hours;
        let c = binomial(n - 1, m);
        let m_fact = factorial(m) as f64;
        f.powi(m as i32 + 1) / ((n as f64) * m_fact * (c as f64) * r.powi(m as i32))
    }

    /// Availability of a single component: `MTTF/(MTTF+MTTR)`.
    pub fn component_availability(&self) -> f64 {
        self.device_mttf_hours / (self.device_mttf_hours + self.repair_mttr_hours)
    }

    /// Pool-level availability lower bound for G independent stripe groups
    /// (each unavailable with probability ≈ (R/F)^(m+1) under the same
    /// rare-event model): `A ≈ 1 - G·(R/F)^(m+1)`.
    pub fn pool_availability(&self, groups: u64) -> f64 {
        let r_over_f = self.repair_mttr_hours / self.device_mttf_hours;
        1.0 - (groups as f64) * r_over_f.powi(self.m as i32 + 1)
    }
}

fn binomial(n: u64, r: u64) -> u64 {
    if r > n {
        return 0;
    }
    let r = r.min(n - r);
    let mut acc: u128 = 1;
    for i in 0..r {
        // C(n, i+1) = C(n, i) · (n-i) / (i+1) — exact at each step.
        acc = acc * (n - i) as u128 / (i + 1) as u128;
    }
    acc as u64
}

fn factorial(n: u64) -> u64 {
    (1..=n).product()
}

// ---------------------------------------------------------------------------
// Zipf cache model (spec §12.3 / audit M8)
// ---------------------------------------------------------------------------

/// Zipfian access with skew `s` over `n_objects`: cache of `c_objects`
/// achieves hit ratio ≈ `(C/N)^(1-s)` for s < 1 (i.i.d. request model).
///
/// Caveats (audit M8): assumes independent requests (no temporal locality —
/// LRU typically does *better* than this bound on real traces) and equal
/// object sizes (extent sizes vary; a byte-weighted model is needed for
/// cache sizing in bytes).
pub fn zipf_hit_ratio(c_objects: f64, n_objects: f64, s: f64) -> f64 {
    assert!((0.0..1.0).contains(&s), "closed-form is for s < 1");
    (c_objects / n_objects).powf(1.0 - s)
}

/// Inverse: smallest cache (in objects) achieving `target` hit ratio.
/// `(C/N)^(1-s) ≥ t  ⇔  C ≥ N · t^(1/(1-s))`.
pub fn zipf_cache_for_target(n_objects: f64, s: f64, target: f64) -> f64 {
    assert!((0.0..1.0).contains(&s));
    assert!((0.0..1.0).contains(&target));
    n_objects * target.powf(1.0 / (1.0 - s))
}

/// Estimate Zipf skew `s` from observed access counts by log-log regression
/// on ranks (a robust MLE approximation for prototype use).
pub fn estimate_zipf_skew(access_counts: &[u64]) -> f64 {
    let mut sorted: Vec<f64> = access_counts.iter().map(|&c| c as f64).collect();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for (i, count) in sorted.iter().enumerate() {
        if *count > 0.0 {
            xs.push((i + 1) as f64);
            ys.push(*count);
        }
    }
    if xs.len() < 2 {
        return 0.86; // neutral default
    }
    // OLS on (log rank, log count): slope = -s.
    let n = xs.len() as f64;
    let lx: Vec<f64> = xs.iter().map(|x| x.ln()).collect();
    let ly: Vec<f64> = ys.iter().map(|y| y.ln()).collect();
    let mean_x = lx.iter().sum::<f64>() / n;
    let mean_y = ly.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..lx.len() {
        num += (lx[i] - mean_x) * (ly[i] - mean_y);
        den += (lx[i] - mean_x).powi(2);
    }
    if den == 0.0 {
        return 0.86;
    }
    let slope = num / den;
    (-slope).clamp(0.0, 0.999)
}

// ---------------------------------------------------------------------------
// Hash-prefix collision risk (audit M6)
// ---------------------------------------------------------------------------

/// Expected number of distinct-prefix collisions among `n` items sharing a
/// `bits`-bit truncated hash (birthday bound): `n² / 2^(bits+1)`.
pub fn expected_prefix_collisions(n: f64, bits: u32) -> f64 {
    (n * n) / 2f64.powi(bits as i32 + 1)
}

// ---------------------------------------------------------------------------
// Scrub scheduling (spec §11)
// ---------------------------------------------------------------------------

/// Scrub schedule inputs.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ScrubParams {
    /// Usable pool bytes.
    pub pool_bytes: f64,
    /// Scrub read bandwidth, bytes/sec (background priority).
    pub scrub_bps: f64,
    /// Safety factor S: full-scrub time must stay ≤ MTTF/S.
    pub safety_factor: f64,
    /// Per-device MTTF in hours.
    pub device_mttf_hours: f64,
}

impl Default for ScrubParams {
    fn default() -> Self {
        Self {
            pool_bytes: 1.0e14, // 100 TiB
            scrub_bps: 200.0e6, // ~200 MB/s
            safety_factor: 10.0,
            device_mttf_hours: 1.0e6,
        }
    }
}

impl ScrubParams {
    /// Wall-clock seconds for one full scrub pass.
    pub fn full_scrub_secs(&self) -> f64 {
        self.pool_bytes / self.scrub_bps
    }

    /// True if the schedule meets the spec's constraint
    /// `FullScrubTime ≤ MTTF / SafetyFactor`.
    pub fn meets_deadline(&self) -> bool {
        let deadline_secs = (self.device_mttf_hours / self.safety_factor) * 3600.0;
        self.full_scrub_secs() <= deadline_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wa_formulas_and_their_disagreement() {
        // The corrected LFS model hits exactly 2× at 50% utilization —
        // matching the spec's own prose target ("~2× at 50%")...
        assert!((wa_lfs(0.5) - 2.0).abs() < 1e-9);
        // ...while the spec's formula gives 4× there (audit M1).
        assert!((wa_spec(0.5) - 4.0).abs() < 1e-9);
        // Journaling every payload byte adds exactly 1× on top (audit C1).
        assert!((wa_with_data_journal(0.5) - 3.0).abs() < 1e-9);
        assert!((wa_with_data_journal(0.5) - wa_lfs(0.5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cleaner_prefers_old_dead_segments() {
        let segs = vec![
            SegmentInfo { live_fraction: 0.9, age_secs: 100.0 }, // mostly live
            SegmentInfo { live_fraction: 0.1, age_secs: 10_000.0 }, // old + dead
            SegmentInfo { live_fraction: 0.2, age_secs: 1_000.0 },
        ];
        let ranked = rank_for_cleaning(&segs);
        assert_eq!(ranked[0], 1, "old dead segment must rank first");
        assert_eq!(ranked[2], 0, "mostly-live segment must rank last");
    }

    #[test]
    fn spec_cost_formula_diverges_from_cost_benefit() {
        // Audit M2, refined: spec §8.1 presents BOTH
        //   benefit/cost = ((1-u)·age)/(1+u)   [Rosenblum-Ousterhout]
        //   Cost(seg)    = (1-u)/age           [minimized]
        // Minimizing the second is equivalent to maximizing age/(1-u) — a
        // *different* policy than maximizing the first. They disagree on
        // this concrete pair:
        let a = SegmentInfo { live_fraction: 0.9, age_secs: 10_000.0 }; // old, mostly live
        let b = SegmentInfo { live_fraction: 0.5, age_secs: 2_000.0 }; // mid everything
        // Rosenblum cost-benefit prefers b (more net benefit):
        assert!(b.benefit_cost() > a.benefit_cost());
        // The spec's "Cost" prefers a (lower cost to minimize):
        assert!(a.cost_spec() < b.cost_spec());
        // => the two spec formulas rank the same segments differently —
        //    an internal inconsistency, whichever one the cleaner follows.
    }

    #[test]
    fn mttdl_spec_example_number_reproduces() {
        // Spec §13.1: k=8,m=3,F=10^6h,R=4h → ≈ 2.96e18 hours.
        let ec = EcReliability::default();
        let v = ec.mttdl_spec();
        assert!((v - 2.96e18).abs() / 2.96e18 < 0.01, "got {v:e}");
        // In years (~8766 h/yr): ≈ 3.4e14 years.
        let years = v / 8766.0;
        assert!((years - 3.4e14).abs() / 3.4e14 < 0.02);
    }

    #[test]
    fn mttdl_ctmc_matches_exact_raid5_case() {
        // For m=1 the CTMC-corrected formula must equal F²/(n(n-1)R).
        let ec = EcReliability { k: 7, m: 1, device_mttf_hours: 1.0e6, repair_mttr_hours: 4.0 };
        let n = ec.n() as f64;
        let exact = (1.0e6f64).powi(2) / (n * (n - 1.0) * 4.0);
        assert!((ec.mttdl_ctmc() - exact).abs() / exact < 1e-12);
        // ...and the spec formula is 2× pessimistic here (audit M4):
        // spec = F^2/(2·n(n-1)·R) — half the true MTTDL.
        assert!((ec.mttdl_ctmc() / ec.mttdl_spec() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn mttdl_grows_with_each_parity_level() {
        let base = EcReliability::default();
        for m in 1..5u32 {
            let smaller = EcReliability { m, ..base };
            let larger = EcReliability { m: m + 1, ..base };
            assert!(larger.mttdl_ctmc() > smaller.mttdl_ctmc() * 100.0);
        }
    }

    #[test]
    fn availability_math() {
        let ec = EcReliability::default();
        assert!(ec.component_availability() > 0.99999);
        // Six nines at pool level for a modest number of groups.
        assert!(ec.pool_availability(10_000) > 0.999_999);
    }

    #[test]
    fn binomial_and_factorial() {
        assert_eq!(binomial(10, 3), 120);
        assert_eq!(binomial(11, 2), 55);
        assert_eq!(binomial(5, 0), 1);
        assert_eq!(factorial(4), 24);
    }

    #[test]
    fn zipf_hit_ratio_and_inverse() {
        // Hit ratio rises with cache, falls with skew.
        let h1 = zipf_hit_ratio(100.0, 1000.0, 0.5);
        let h2 = zipf_hit_ratio(500.0, 1000.0, 0.5);
        assert!(h2 > h1);
        assert!(zipf_hit_ratio(100.0, 1000.0, 0.9) > zipf_hit_ratio(100.0, 1000.0, 0.5));

        // Inverse solves the SLA sizing problem.
        let needed = zipf_cache_for_target(1.0e6, 0.8, 0.9);
        let achieved = zipf_hit_ratio(needed, 1.0e6, 0.8);
        assert!(achieved >= 0.9);
        assert!(achieved < 0.9 + 1e-6, "should be tight, got {achieved}");
    }

    #[test]
    fn zipf_estimator_recovers_skew() {
        // Generate Zipf-ish counts for s = 0.8 over 500 ranks.
        let s = 0.8f64;
        let counts: Vec<u64> = (1..=500u32)
            .map(|r| (1.0e6 / (r as f64).powf(s)) as u64)
            .collect();
        let est = estimate_zipf_skew(&counts);
        assert!((est - s).abs() < 0.05, "estimated s = {est}");
    }

    #[test]
    fn prefix_collision_birthday_math() {
        // 10^12 unique chunks with 64-bit prefixes: ≈ 2.7e4 collisions —
        // the reason ExtentDescriptor prefixes are locators, not identity
        // (audit M6).
        let c = expected_prefix_collisions(1.0e12, 64);
        assert!((c - 2.7e4).abs() / 2.7e4 < 0.05, "got {c:e}");
        // 32-bit prefixes at the same scale: catastrophic.
        assert!(expected_prefix_collisions(1.0e12, 32) > 1.0e14);
    }

    #[test]
    fn scrub_deadline_math() {
        let p = ScrubParams::default();
        // 1e14 bytes at 2e8 B/s = 5e5 s = 5.787 days.
        let days = p.full_scrub_secs() / 86400.0;
        assert!((days - 5.79).abs() < 0.05, "days = {days}");
        assert!(p.meets_deadline());

        // Audit observation: the spec's constraint FullScrubTime ≤ MTTF/10
        // is nearly vacuous for modern drives — MTTF 10^6 h / 10 = 11.4
        // YEARS. A 10 PiB pool at 200 MB/s (5.79 days/PiB) still "meets" it:
        let ten_pi = ScrubParams { pool_bytes: 1.0e16, ..p };
        assert!(
            ten_pi.meets_deadline(),
            "10 PiB pool still meets MTTF/10 — the deadline is ~11 years"
        );
        // It takes an exabyte-scale pool at this rate to actually violate:
        let exa = ScrubParams { pool_bytes: 1.0e18, ..p };
        assert!(!exa.meets_deadline());
        // The *binding* constraint in practice is latent-error interception
        // (scrub must outrun URE accumulation between RAID-failure events),
        // which the spec does not model — see audit F4.
    }
}
