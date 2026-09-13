//! Pool reliability mathematics for the Guardian plane (LionFS 8.0 —
//! the HFS merge).
//!
//! The local engine already had the Weibull per-drive hazard model
//! (`guardian::failure`); what it lacked is the POOL-level view: how
//! the redundancy layout itself converts device MTTF/MTTR into
//! mean-time-to-data-loss and availability. The merged HFS cluster
//! plane brings exactly that, battle-tested against the audit's
//! corrected formulas:
//!
//! * `MTTDL ≈ F^(m+1) / (n · m! · C(n-1, m) · R^m)` — the corrected
//!   CTMC-matching rare-event model (the naive spec constant is off
//!   by `m!·(m+1)`: 2× pessimistic at m=1, 1.5× optimistic at m=3);
//! * `WA = 1/(1-u)` — the corrected write-amplification model;
//! * Zipf cache sizing for the tier plane.
//!
//! This module maps every LionFS [`RaidProfile`] onto (k, m)
//! erasure-coding parameters and evaluates the models, so the
//! Guardian and `lfs_predict` can reason about a MOUNTED image's
//! redundancy in the same units as the cluster plane.

use crate::pool::raid::RaidProfile;

// Re-export the cluster plane's verified models (the merge point).
pub use lionfs_cluster::reliability::{
    estimate_zipf_skew, wa_lfs, wa_spec, wa_with_data_journal, EcReliability, ScrubParams,
    SegmentInfo,
};

/// Erasure-coding parameters derived from a mounted pool's profile.
#[derive(Clone, Copy, Debug)]
pub struct EcLayout {
    /// Data shards per stripe group.
    pub k: u32,
    /// Parity shards per stripe group.
    pub m: u32,
    /// Independent stripe groups (Raid10 mirrors; 1 otherwise).
    pub groups: u32,
}

/// Map a LionFS RAID profile + device count onto (k, m, groups).
///
/// * `Single` — one device, no redundancy (m=0): MTTDL = F.
/// * `Raid0`  — n devices, no redundancy: MTTDL = F/n.
/// * `Raid1`  — n-way mirror: loss needs all n copies (m = n-1).
/// * `Raid5`  — one parity shard per stripe (m=1).
/// * `Raid6`  — two parity shards per stripe (m=2).
/// * `Raid10` — G = n/2 independent mirrored pairs, each (k=1, m=1).
pub fn ec_layout(profile: RaidProfile, devices: u32) -> EcLayout {
    let n = devices.max(1);
    match profile {
        RaidProfile::Single => EcLayout { k: 1, m: 0, groups: 1 },
        RaidProfile::Raid0 => EcLayout { k: n, m: 0, groups: 1 },
        RaidProfile::Raid1 => EcLayout { k: 1, m: n.saturating_sub(1), groups: 1 },
        RaidProfile::Raid5 => EcLayout { k: n.saturating_sub(1), m: 1, groups: 1 },
        RaidProfile::Raid6 => EcLayout { k: n.saturating_sub(2), m: 2, groups: 1 },
        RaidProfile::Raid10 => {
            let groups = (n / 2).max(1);
            EcLayout { k: 1, m: 1, groups }
        }
    }
}

/// Pool-level reliability summary for one mounted image.
#[derive(Clone, Copy, Debug)]
pub struct PoolReliability {
    pub profile: RaidProfile,
    pub devices: u32,
    /// The erasure layout the profile maps onto.
    pub layout: EcLayout,
    /// Underlying per-device reliability inputs.
    pub ec: EcReliability,
    /// MTTDL under the naive spec constant (kept for comparison; the
    /// audit showed it is off by `m!·(m+1)` versus the CTMC result).
    pub mttdl_spec_hours: f64,
    /// MTTDL under the corrected CTMC-matching model — the number to
    /// report. For multi-group layouts (Raid10) this is the
    /// per-group MTTDL divided by the group count.
    pub mttdl_ctmc_hours: f64,
    /// Single-component availability `F/(F+R)`.
    pub component_availability: f64,
    /// Pool availability lower bound `1 - G·(R/F)^(m+1)`.
    pub pool_availability: f64,
}

/// Assess a pool: `profile` + `devices` + per-device MTTF/MTTR (hours).
///
/// Defaults mirror the spec's reference drives (MTTF 10^6 h ≈ 114
/// years, MTTR 4 h) when callers pass none.
pub fn assess_pool(
    profile: RaidProfile,
    devices: u32,
    mttf_hours: f64,
    mttr_hours: f64,
) -> PoolReliability {
    let layout = ec_layout(profile, devices);
    let ec = EcReliability {
        k: layout.k,
        m: layout.m,
        device_mttf_hours: mttf_hours,
        repair_mttr_hours: mttr_hours,
    };
    let mttdl_ctmc = if layout.groups > 1 {
        // Independent groups fail in parallel: the pool's loss rate is
        // the sum of per-group loss rates (rare events add).
        ec.mttdl_ctmc() / layout.groups as f64
    } else {
        ec.mttdl_ctmc()
    };
    PoolReliability {
        profile,
        devices: devices.max(1),
        layout,
        ec,
        mttdl_spec_hours: ec.mttdl_spec(),
        mttdl_ctmc_hours: mttdl_ctmc,
        component_availability: ec.component_availability(),
        pool_availability: ec.pool_availability(layout.groups as u64),
    }
}

/// Write amplification at a measured utilization `u = used/total`.
///
/// Returns `(wa_lfs, wa_spec)`: the CoW/copy-on-write B-ε engine's
/// amplification (1/(1-u), the corrected model) and the naive
/// log-structured model the spec originally used (1/u·... — kept for
/// comparison). Values are `None` outside (0, 1).
pub fn write_amplification(utilization: f64) -> Option<(f64, f64)> {
    if !(0.0..1.0).contains(&utilization) {
        return None;
    }
    Some((wa_lfs(utilization), wa_spec(utilization)))
}

/// Format a duration in hours into human units (years preferred).
pub fn format_hours(hours: f64) -> String {
    const YEAR: f64 = 8760.0; // 365 d
    if hours.is_finite() && hours > 0.0 {
        if hours >= YEAR {
            format!("{hours:.3} h (~{:.1} years)", hours / YEAR)
        } else if hours >= 1.0 {
            format!("{hours:.3} h")
        } else {
            let minutes = hours * 60.0;
            format!("{hours:.3} h (~{minutes:.1} minutes)")
        }
    } else {
        "∞".to_string()
    }
}

/// Prometheus text exposition for a pool assessment (the Guardian
/// telemetry convention: bounded, deterministic series).
pub fn render_prometheus(r: &PoolReliability, utilization: Option<f64>) -> String {
    let mut out = String::new();
    out.push_str("# HELP lionfs_pool_mttdl_hours Mean time to data loss (corrected CTMC model).\n");
    out.push_str("# TYPE lionfs_pool_mttdl_hours gauge\n");
    out.push_str(&format!(
        "lionfs_pool_mttdl_hours{{profile=\"{:?}\"}} {:.6}\n",
        r.profile, r.mttdl_ctmc_hours
    ));
    out.push_str("# HELP lionfs_pool_availability Pool availability lower bound.\n");
    out.push_str("# TYPE lionfs_pool_availability gauge\n");
    out.push_str(&format!(
        "lionfs_pool_availability{{profile=\"{:?}\"}} {:.12}\n",
        r.profile, r.pool_availability
    ));
    if let Some(u) = utilization {
        if let Some((lfs, spec)) = write_amplification(u) {
            out.push_str("# HELP lionfs_write_amplification Write amplification at current utilization.\n");
            out.push_str("# TYPE lionfs_write_amplification gauge\n");
            out.push_str(&format!(
                "lionfs_write_amplification{{model=\"cow\"}} {:.6}\n",
                lfs
            ));
            out.push_str(&format!(
                "lionfs_write_amplification{{model=\"log_structured\"}} {:.6}\n",
                spec
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: f64 = 1.0e6; // spec reference MTTF
    const R: f64 = 4.0; // spec reference MTTR

    #[test]
    fn single_device_mttdl_is_mttf() {
        let r = assess_pool(RaidProfile::Single, 1, F, R);
        assert!((r.mttdl_ctmc_hours - F).abs() < 1.0, "n=1, m=0: MTTDL = F, got {}", r.mttdl_ctmc_hours);
        // Availability of one component.
        assert!((r.component_availability - F / (F + R)).abs() < 1e-12);
    }

    #[test]
    fn raid0_divides_by_devices() {
        let r = assess_pool(RaidProfile::Raid0, 4, F, R);
        assert!((r.mttdl_ctmc_hours - F / 4.0).abs() < 1.0, "Raid0 MTTDL = F/n");
    }

    #[test]
    fn raid5_matches_exact_ctmc() {
        // The audit's M1/M4 anchor: m=1 gives F^2/(n(n-1)R) exactly.
        let r = assess_pool(RaidProfile::Raid5, 3, F, R);
        let exact = F * F / (3.0 * 2.0 * R);
        assert!(
            (r.mttdl_ctmc_hours - exact).abs() / exact < 1e-9,
            "Raid5(3): CTMC formula must equal F^2/(n(n-1)R)"
        );
        // And the naive spec constant is 2x pessimistic here.
        assert!((r.mttdl_spec_hours - exact / 2.0).abs() / exact < 1e-9);
    }

    #[test]
    fn raid6_uses_two_parity() {
        let r = assess_pool(RaidProfile::Raid6, 4, F, R);
        assert_eq!(r.layout.m, 2);
        assert_eq!(r.layout.k, 2);
        // F^3 / (4 · 2! · C(3,2) · R^2) = F^3/(4·2·3·16)
        let expect = F * F * F / (4.0 * 2.0 * 3.0 * R * R);
        assert!((r.mttdl_ctmc_hours - expect).abs() / expect < 1e-9);
    }

    #[test]
    fn raid10_scales_by_group_count() {
        let r = assess_pool(RaidProfile::Raid10, 4, F, R);
        assert_eq!(r.layout.groups, 2);
        // Per-pair m=1: F^2/(2·1·C(1,1)·R) = F^2/(2R); two groups halve it.
        let expect = F * F / (2.0 * R) / 2.0;
        assert!((r.mttdl_ctmc_hours - expect).abs() / expect < 1e-9);
    }

    #[test]
    fn raid1_full_mirror_loss() {
        let r = assess_pool(RaidProfile::Raid1, 3, F, R);
        assert_eq!(r.layout.m, 2); // all three copies must be lost
        let expect = F * F * F / (3.0 * 2.0 * 1.0 * R * R);
        assert!((r.mttdl_ctmc_hours - expect).abs() / expect < 1e-9);
    }

    #[test]
    fn write_amplification_models() {
        // u -> 0: the corrected CoW model -> 1; the (erroneous, kept
        // for comparison) spec model floors at 2.
        let (l0, s0) = write_amplification(0.001).unwrap();
        assert!(l0 < 1.01, "corrected model approaches 1, got {l0}");
        assert!((s0 - 2.0).abs() < 0.01, "spec model floors at 2, got {s0}");
        // The corrected CoW model is exactly 1/(1-u).
        let (l, _) = write_amplification(0.75).unwrap();
        assert!((l - 4.0).abs() < 1e-12);
        // Out of range is rejected.
        assert!(write_amplification(1.0).is_none());
        assert!(write_amplification(-0.1).is_none());
    }

    #[test]
    fn prometheus_rendering_is_stable() {
        let r = assess_pool(RaidProfile::Raid5, 3, F, R);
        let text = render_prometheus(&r, Some(0.5));
        assert!(text.contains("lionfs_pool_mttdl_hours"));
        assert!(text.contains("lionfs_pool_availability"));
        assert!(text.contains("lionfs_write_amplification{model=\"cow\"} 2"));
        // Deterministic render (same input, same bytes).
        assert_eq!(text, render_prometheus(&r, Some(0.5)));
    }

    #[test]
    fn format_hours_units() {
        assert!(format_hours(24.0 * 365.0).contains("years"));
        assert!(format_hours(5.0).contains("h"));
        assert!(format_hours(0.01).contains("minutes"));
        assert_eq!(format_hours(f64::NAN), "∞");
    }
}
