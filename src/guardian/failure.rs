//! Drive-failure prediction (RFC-004 §7.2): a Weibull hazard model over
//! SMART-style telemetry.
//!
//! Why Weibull and not a neural net: drive wear-out is the textbook
//! Weibull process (increasing hazard with age, shape k > 1), the
//! parameters are three numbers an operator can audit, and inference
//! is a multiply. Backblaze's published field data shows realloc /
//! pending-sector / CRC-error counts carrying most of the predictive
//! weight; the model here is the honest, tiny version of that: a
//! baseline annualized hazard bumped by weighted telemetry counters.
//!
//! Two deliberately separate signals:
//!
//! * the **telemetry multiplier** (100 = clean drive) drives the
//!   [`RiskBand`] -- counters are the strong, near-term signal;
//! * the **age/Weibull baseline** only modulates the remaining-life
//!   estimate -- age is a prior, not an alarm.
//!
//! The output is a band plus a remaining-life point estimate in
//! power-on hours. Guardian emits *advisories* ("migrate this device
//! within N days"), never unilateral device removals.

/// SMART-style counters for one device, as exported by the PAL and
/// accumulated since observation start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriveTelemetry {
    /// Reallocated sectors count.
    pub realloc_events: u64,
    /// Current pending (unresolved) sectors.
    pub pending_sectors: u64,
    /// Interface CRC / UDMA error count (cable/controller suspects).
    pub crc_errors: u64,
    /// Median device latency at the last sample, microseconds.
    pub median_latency_us: u64,
    /// 99th-percentile latency at the last sample, microseconds.
    pub p99_latency_us: u64,
    /// Power-on hours.
    pub power_on_hours: u64,
    /// Scrub passes that found and repaired at least one sector.
    pub scrub_repairs: u64,
}

/// Coarse risk classification; the bands drive advisory severity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RiskBand {
    /// Telemetry clean: baseline only.
    Healthy,
    /// Elevated telemetry: watch, no action.
    Watch,
    /// Materially elevated: plan migration on the order of weeks.
    Degraded,
    /// High hazard: migrate on the order of days; treat as failing.
    Failing,
}

impl RiskBand {
    /// Telemetry-multiplier band edges (x100): Healthy < 150,
    /// Watch < 400, Degraded < 1000, Failing >= 1000.
    pub const HEALTHY_MAX: u64 = 150;
    pub const WATCH_MAX: u64 = 400;
    pub const DEGRADED_MAX: u64 = 1_000;

    #[must_use]
    pub fn from_multiplier(x100: u64) -> Self {
        if x100 < Self::HEALTHY_MAX {
            Self::Healthy
        } else if x100 < Self::WATCH_MAX {
            Self::Watch
        } else if x100 < Self::DEGRADED_MAX {
            Self::Degraded
        } else {
            Self::Failing
        }
    }

    /// Human-readable name (advisory text).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Watch => "watch",
            Self::Degraded => "degraded",
            Self::Failing => "failing",
        }
    }
}

/// `ln(2)` in 16.16.
const LN2_16_16: i64 = 45_426;
/// `ln(2) * 65536` for the median-life constant: 0.6931 * 65536.
const LN2_Q16: u64 = 45_426;
/// One year of power-on hours.
const HOURS_PER_YEAR: u64 = 8_760;

/// `(num/den)^(k-1)` in 16.16 for positive integers, shape `k` given
/// in x100. Computed as `exp((k-1) * ln(num/den))` with a 3-term
/// Taylor expansion (max error ~2% over the realistic parameter
/// space), which keeps the shape parameter genuinely tunable.
fn ratio_pow_km1_16_16(num: u64, den: u64, shape_x100: u64) -> u64 {
    if num == 0 || den == 0 {
        return 0;
    }
    // ln(num/den) = ln2 * (log2(num) - log2(den)), each log2 from
    // floor + 16-step fractional part (see entropy.rs for the table
    // rationale; duplicated here to keep modules independent).
    const FRACT16: [u32; 16] = [
        0, 5_732, 11_134, 16_241, 21_087, 25_706, 30_102, 34_303, 38_328, 42_186, 45_890,
        49_458, 52_896, 56_211, 59_417, 62_516,
    ];
    let log2_q = |x: u64| -> i64 {
        let f = 63 - u64::from(x.leading_zeros());
        let base = 1u64 << f;
        let j = (((x - base) as u128 * 16) / base as u128).min(15) as usize;
        ((f as i64) << 16) + i64::from(FRACT16[j])
    };
    let log2_ratio = log2_q(num) - log2_q(den); // signed 16.16
    // u = (k-1) * ln(ratio) in signed 16.16.
    let km1 = (shape_x100.saturating_sub(100)) as i64; // (k-1) in x100
    let u = (i128::from(km1) * i128::from(log2_ratio) * i128::from(LN2_16_16))
        / (100 * 65_536); // /100 for x100, /65536 to collapse one 16.16
    // exp(u) via Taylor: 1 + u + u^2/2 + u^3/6 (u in 16.16).
    let one = 1i128 << 16;
    let u2 = (u * u) / (2 * 65_536);
    let u3 = (u * u * u) / (6 * 65_536 * 65_536);
    let val = one + u + u2 + u3;
    let val = val.clamp(1, 8 << 16); // (0, 8.0] in 16.16, clamped sane
    val as u64
}

/// The fitted model. Defaults are field-calibrated starting points
/// (RFC-004 §7.2, Table 3); operators retune via config.
#[derive(Clone, Copy, Debug)]
pub struct FailurePredictor {
    /// Weibull shape k in x100 (k > 100 = wear-out). 130 is
    /// conservative for HDD populations; flash trends higher.
    pub shape_x100: u64,
    /// Characteristic life eta in power-on hours.
    pub eta_hours: u64,
    /// Hazard multiplier per reallocated sector (x100).
    pub per_realloc_x100: u64,
    /// Hazard multiplier per pending sector (x100).
    pub per_pending_x100: u64,
    /// Hazard multiplier per CRC error (x100).
    pub per_crc_x100: u64,
    /// Hazard multiplier per scrub repair event (x100).
    pub per_scrub_repair_x100: u64,
    /// Hazard multiplier per latency-inflation point (x100); one point
    /// per +0.2 of p99/median above 2.0.
    pub per_latency_inflation_x100: u64,
}

impl Default for FailurePredictor {
    fn default() -> Self {
        Self {
            shape_x100: 130,
            eta_hours: 80_000, // ~9 years characteristic life
            per_realloc_x100: 40,      // +0.4x baseline per event
            per_pending_x100: 80,      // pending is worse than reallocated
            per_crc_x100: 10,          // often a cable, but count it
            per_scrub_repair_x100: 60, // repaired = happened twice-ish
            per_latency_inflation_x100: 5,
        }
    }
}

/// The full risk assessment for one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RiskAssessment {
    pub band: RiskBand,
    /// Telemetry hazard multiplier, x100 (100 = clean).
    pub hazard_multiplier_x100: u64,
    /// Annualized effective hazard (baseline x multiplier), x100 of
    /// 100%/year (100 = expect one failure per year).
    pub annual_hazard_x100: u64,
    /// Point estimate of median remaining life in power-on hours
    /// (saturating at 100 years; a healthy drive is a model, not a
    /// promise).
    pub est_remaining_hours: u64,
}

impl FailurePredictor {
    /// Baseline annualized Weibull hazard at `power_on_hours`, x100:
    /// `h = k * (8760/eta) * (t/eta)^(k-1)`.
    #[must_use]
    pub fn baseline_hazard_x100(&self, power_on_hours: u64) -> u64 {
        let pow = ratio_pow_km1_16_16(power_on_hours, self.eta_hours, self.shape_x100);
        let eta = self.eta_hours.max(1) as u128;
        let h = (u128::from(self.shape_x100) * u128::from(HOURS_PER_YEAR) * u128::from(pow))
            / (eta * 65_536);
        h.clamp(1, 1_000_000) as u64
    }

    /// The telemetry multiplier: 100 plus weighted counters. Pure
    /// function of the counters; age does not touch this.
    #[must_use]
    pub fn telemetry_multiplier_x100(&self, t: &DriveTelemetry) -> u64 {
        let mult = 100u64
            .saturating_add(t.realloc_events.saturating_mul(self.per_realloc_x100))
            .saturating_add(t.pending_sectors.saturating_mul(self.per_pending_x100))
            .saturating_add(t.crc_errors.saturating_mul(self.per_crc_x100))
            .saturating_add(t.scrub_repairs.saturating_mul(self.per_scrub_repair_x100));
        // Latency inflation: p99/median above 2.0 counts points.
        if t.median_latency_us > 0 {
            let ratio_x10 = (t.p99_latency_us * 10) / t.median_latency_us;
            let points = ratio_x10.saturating_sub(20) / 2;
            mult.saturating_add(points.saturating_mul(self.per_latency_inflation_x100))
        } else {
            mult
        }
    }

    /// Full assessment.
    #[must_use]
    pub fn assess(&self, t: &DriveTelemetry) -> RiskAssessment {
        let mult = self.telemetry_multiplier_x100(t);
        let base = self.baseline_hazard_x100(t.power_on_hours);
        let annual = base.saturating_mul(mult) / 100;
        RiskAssessment {
            band: RiskBand::from_multiplier(mult),
            hazard_multiplier_x100: mult,
            annual_hazard_x100: annual,
            est_remaining_hours: self.remaining_hours(annual),
        }
    }

    /// Convenience: just the band.
    #[must_use]
    pub fn hazard_band(&self, t: &DriveTelemetry) -> RiskBand {
        self.assess(t).band
    }

    /// Median remaining life in power-on hours given the annualized
    /// effective hazard (x100): `ln(2) / h`. Saturating at 100 years;
    /// zero hazard (absurd inputs) also saturates rather than dividing.
    #[must_use]
    pub fn remaining_hours(&self, annual_hazard_x100: u64) -> u64 {
        if annual_hazard_x100 == 0 {
            return 876_000;
        }
        // ln(2) is 16.16 -> divide by 65536 to land in whole hours:
        // hours = LN2_Q16 * 8760 * 100 / (h_x100 * 65536).
        let median = (u128::from(LN2_Q16) * u128::from(HOURS_PER_YEAR) * 100)
            / (u128::from(annual_hazard_x100) * 65_536);
        u64::try_from(median.min(876_000)).unwrap_or(876_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy(hours: u64) -> DriveTelemetry {
        DriveTelemetry {
            realloc_events: 0,
            pending_sectors: 0,
            crc_errors: 0,
            median_latency_us: 500,
            p99_latency_us: 900, // ratio 1.8: no inflation
            power_on_hours: hours,
            scrub_repairs: 0,
        }
    }

    #[test]
    fn young_clean_drive_is_healthy() {
        let p = FailurePredictor::default();
        let a = p.assess(&healthy(5_000));
        assert_eq!(a.band, RiskBand::Healthy);
        assert_eq!(a.hazard_multiplier_x100, 100);
        // Baseline at 5k hours: (t/eta)=0.0625, ^0.3 ~= 0.435:
        // h = 130*8760*0.435/80000 ~= 6.2 -> ~98k h median remaining.
        assert!(a.est_remaining_hours > 50_000, "remaining {}", a.est_remaining_hours);
        assert!(a.est_remaining_hours <= 876_000);
    }

    #[test]
    fn realloc_and_pending_escalate() {
        let p = FailurePredictor::default();
        let mut t = healthy(40_000);
        t.realloc_events = 5; // +200
        t.pending_sectors = 3; // +240
        t.scrub_repairs = 1; // +60
        let a = p.assess(&t);
        assert_eq!(a.hazard_multiplier_x100, 600);
        assert_eq!(a.band, RiskBand::Degraded);
    }

    #[test]
    fn failing_drive_band() {
        let p = FailurePredictor::default();
        let t = DriveTelemetry {
            realloc_events: 40,
            pending_sectors: 20,
            crc_errors: 50,
            median_latency_us: 200,
            p99_latency_us: 1_200, // ratio 6.0 -> 20 inflation points
            power_on_hours: 60_000,
            scrub_repairs: 5,
        };
        let a = p.assess(&t);
        // 100 + 1600 + 1600 + 500 + 300 + 100 = 4200.
        assert_eq!(a.hazard_multiplier_x100, 4_200);
        assert_eq!(a.band, RiskBand::Failing);
        // Failing drives advise weeks, not years: annual hazard is
        // baseline(~13) * 41 ~= 540 -> median ~ 46 days.
        assert!(a.est_remaining_hours < 90 * 24, "remaining {}", a.est_remaining_hours);
    }

    #[test]
    fn band_edges_are_stable() {
        assert_eq!(RiskBand::from_multiplier(0), RiskBand::Healthy);
        assert_eq!(RiskBand::from_multiplier(149), RiskBand::Healthy);
        assert_eq!(RiskBand::from_multiplier(150), RiskBand::Watch);
        assert_eq!(RiskBand::from_multiplier(399), RiskBand::Watch);
        assert_eq!(RiskBand::from_multiplier(400), RiskBand::Degraded);
        assert_eq!(RiskBand::from_multiplier(999), RiskBand::Degraded);
        assert_eq!(RiskBand::from_multiplier(1_000), RiskBand::Failing);
        assert_eq!(RiskBand::Failing.name(), "failing");
        assert_eq!(RiskBand::Healthy.name(), "healthy");
    }

    #[test]
    fn latency_inflation_counts() {
        let p = FailurePredictor::default();
        let mut t = healthy(10_000);
        t.median_latency_us = 100;
        t.p99_latency_us = 1_000; // ratio 10.0 -> 40 points -> +200
        assert_eq!(p.telemetry_multiplier_x100(&t), 300);
    }

    #[test]
    fn zero_median_latency_is_handled() {
        // No latency data: the multiplier must not divide by zero.
        let p = FailurePredictor::default();
        let mut t = healthy(10_000);
        t.median_latency_us = 0;
        t.p99_latency_us = 0;
        assert_eq!(p.telemetry_multiplier_x100(&t), 100);
        assert_eq!(p.hazard_band(&t), RiskBand::Healthy);
    }

    #[test]
    fn remaining_life_decreases_with_hazard() {
        let p = FailurePredictor::default();
        let r1 = p.remaining_hours(6);
        let r2 = p.remaining_hours(30);
        let r3 = p.remaining_hours(300);
        assert!(r1 > r2);
        assert!(r2 > r3);
        // And saturates rather than panicking at absurd inputs.
        assert!(p.remaining_hours(0) <= 876_000);
        assert!(p.remaining_hours(u64::MAX / 2) <= 876_000);
    }

    #[test]
    fn baseline_hazard_is_monotone_in_age() {
        let p = FailurePredictor::default();
        let h1 = p.baseline_hazard_x100(10_000);
        let h2 = p.baseline_hazard_x100(40_000);
        let h3 = p.baseline_hazard_x100(80_000);
        assert!(h1 < h2, "{h1} {h2}");
        assert!(h2 < h3, "{h2} {h3}");
        assert!(h1 > 0 && h3 < 1_000_000);
    }

    #[test]
    fn shape_parameter_is_honored() {
        // A higher shape at the same age must yield higher baseline
        // hazard (steeper wear-out).
        let mut p = FailurePredictor::default();
        let h1 = p.baseline_hazard_x100(50_000);
        p.shape_x100 = 180;
        let h2 = p.baseline_hazard_x100(50_000);
        assert!(h2 > h1, "{h1} {h2}");
    }

    #[test]
    fn ratio_power_is_sane() {
        // (eta/eta)^(k-1) == 1 for any shape.
        for shape in [100u64, 130, 180] {
            let v = ratio_pow_km1_16_16(80_000, 80_000, shape);
            assert_eq!(v, 1 << 16, "shape {shape}");
        }
        // num < den -> value < 1; num > den -> value > 1 (for k > 1).
        let v = ratio_pow_km1_16_16(40_000, 80_000, 130);
        assert!(v < 1 << 16, "{v}");
        let v = ratio_pow_km1_16_16(160_000, 80_000, 130);
        assert!(v > 1 << 16, "{v}");
        // k = 1 exactly: the power is 1 regardless of ratio.
        let v = ratio_pow_km1_16_16(40_000, 80_000, 100);
        assert_eq!(v, 1 << 16);
        // Degenerate inputs do not panic.
        assert_eq!(ratio_pow_km1_16_16(0, 80_000, 130), 0);
        assert_eq!(ratio_pow_km1_16_16(80_000, 0, 130), 0);
    }
}
