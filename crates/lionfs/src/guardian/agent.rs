//! The Guardian agent loop (RFC-004 §7.4): the policy engine that
//! turns detector evidence into advisories and out-of-band actions.
//!
//! The design rule, stated once and enforced everywhere: the agent
//! never touches the data path. Its outputs are [`Advisory`] records
//! on an advisory bus (a bounded ring the daemon drains), and its
//! actions are *policy* operations -- snapshot-freeze, scrub-priority,
//! tier retunes, QoS class reassignment -- all of which run through
//! the ordinary control-plane APIs, all of which are logged, and all
//! of which are reversible. A filesystem whose recovery depends on a
//! model is a filesystem you cannot crash-test; LionFS keeps the
//! model in the observatory and the proofs in the kernel.

use std::collections::VecDeque;

use super::entropy::Suspicion;
use super::failure::RiskAssessment;
use super::workload::StreamClass;

/// What kind of advisory this is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdvisoryKind {
    /// Ransomware suspicion crossed the freeze line.
    RansomwareSuspicion,
    /// Drive failure risk band changed (or first assessed).
    DriveRisk { band: super::failure::RiskBand },
    /// Workload profile changed.
    WorkloadShift { class: StreamClass },
}

impl AdvisoryKind {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::RansomwareSuspicion => "ransomware-suspicion",
            Self::DriveRisk { .. } => "drive-risk",
            Self::WorkloadShift { .. } => "workload-shift",
        }
    }

    /// The rate-limit key: includes the discriminating data so that an
    /// *escalation* (a worse band, a different workload class) is never
    /// suppressed as a "repeat" of the previous advisory.
    fn rate_key(&self, device: u64) -> String {
        match self {
            Self::RansomwareSuspicion => format!("{}:{device}", self.name()),
            Self::DriveRisk { band } => format!("{}:{band:?}:{device}", self.name()),
            Self::WorkloadShift { class } => format!("{}:{}:{device}", self.name(), class.tag()),
        }
    }
}

/// What the agent wants done. All reversible; all logged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentAction {
    /// Freeze the snapshot schedule (create one now, hold the rotation)
    /// so the post-encrypt state cannot evict pre-encrypt recovery
    /// points.
    FreezeSnapshots,
    /// Bump this device's scrub priority and schedule a full verify.
    EscalateScrub,
    /// Begin planning data migration off the device (pool rebalance).
    PlanMigration,
    /// Apply the policy retune implied by the new workload class.
    RetunePolicies,
    /// Log-only: evidence below action thresholds.
    LogOnly,
}

/// One emitted advisory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advisory {
    pub kind: AdvisoryKind,
    pub action: AgentAction,
    /// Score/evidence summary for the audit trail (bps for suspicion,
    /// multiplier x100 for drive risk, 0 for workload).
    pub evidence: u64,
    /// Monotonic window counter when emitted.
    pub window: u64,
}

/// Agent tunables.
#[derive(Clone, Copy, Debug)]
pub struct AgentConfig {
    /// Suspicion score (bps) at which snapshots freeze.
    pub freeze_bps: u32,
    /// Drive-risk band at which migration planning starts.
    pub migration_band: super::failure::RiskBand,
    /// Advisory ring capacity (bounded; the daemon drains it).
    pub ring_capacity: usize,
    /// Minimum windows between repeated advisories of the same kind
    /// (rate limit so a flapping detector cannot flood the bus).
    pub repeat_window_gap: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            freeze_bps: super::entropy::FREEZE_BPS,
            migration_band: super::failure::RiskBand::Degraded,
            ring_capacity: 256,
            repeat_window_gap: 5,
        }
    }
}

/// The agent: consumes per-window detector outputs, emits advisories.
pub struct Agent {
    config: AgentConfig,
    ring: VecDeque<Advisory>,
    window: u64,
    /// (kind-name, window) of the last emission, for rate limiting.
    last_emitted: Vec<(String, u64)>,
    last_workload: Option<StreamClass>,
}

impl Agent {
    #[must_use]
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            ring: VecDeque::with_capacity(config.ring_capacity),
            window: 0,
            last_emitted: Vec::new(),
            last_workload: None,
        }
    }

    /// Advances the window counter; call once per observation window
    /// before feeding detector results.
    pub fn tick(&mut self) {
        self.window += 1;
    }

    /// Feeds the entropy watcher's verdict.
    pub fn observe_suspicion(&mut self, s: Suspicion) {
        if s.freeze_recommended {
            self.emit(
                AdvisoryKind::RansomwareSuspicion,
                AgentAction::FreezeSnapshots,
                u64::from(s.score_bps),
            );
        } else if s.score_bps >= self.config.freeze_bps / 2 {
            // Halfway: log-only, so operators see the ramp.
            self.emit(
                AdvisoryKind::RansomwareSuspicion,
                AgentAction::LogOnly,
                u64::from(s.score_bps),
            );
        }
    }

    /// Feeds the drive-risk assessment for one device.
    pub fn observe_drive(&mut self, device: u64, a: &RiskAssessment) {
        use super::failure::RiskBand;
        let band = a.band;
        let action = match band {
            RiskBand::Failing => AgentAction::PlanMigration,
            RiskBand::Degraded => AgentAction::EscalateScrub,
            RiskBand::Watch | RiskBand::Healthy => AgentAction::LogOnly,
        };
        // Migration threshold is configurable; Failing always migrates.
        if band == RiskBand::Degraded
            && self.config.migration_band == RiskBand::Degraded
        {
            // Degraded with migration threshold at Degraded: plan now.
            self.emit_with_device(
                device,
                AdvisoryKind::DriveRisk { band },
                AgentAction::PlanMigration,
                a.hazard_multiplier_x100,
            );
            return;
        }
        self.emit_with_device(
            device,
            AdvisoryKind::DriveRisk { band },
            action,
            a.hazard_multiplier_x100,
        );
    }

    /// Feeds the workload classifier's current class.
    pub fn observe_workload(&mut self, class: StreamClass) {
        if self.last_workload != Some(class) {
            self.last_workload = Some(class);
            self.emit(
                AdvisoryKind::WorkloadShift { class },
                AgentAction::RetunePolicies,
                0,
            );
        }
    }

    /// Core emission with rate limiting and bounded ring.
    fn emit(&mut self, kind: AdvisoryKind, action: AgentAction, evidence: u64) {
        self.emit_with_device(0, kind, action, evidence);
    }

    fn emit_with_device(&mut self, device: u64, kind: AdvisoryKind, action: AgentAction, evidence: u64) {
        let key = kind.rate_key(device);
        if let Some(&(_, w)) = self.last_emitted.iter().find(|(k, _)| *k == key) {
            if self.window.saturating_sub(w) < self.config.repeat_window_gap {
                return; // rate limited: flapping detector protection
            }
        }
        self.last_emitted.retain(|(k, _)| *k != key);
        self.last_emitted.push((key, self.window));
        let advisory = Advisory {
            kind,
            action,
            evidence,
            window: self.window,
        };
        self.ring.push_back(advisory);
        if self.ring.len() > self.config.ring_capacity {
            self.ring.pop_front();
        }
    }

    /// Drains the advisory bus (returns and clears pending advisories).
    pub fn drain(&mut self) -> Vec<Advisory> {
        self.ring.drain(..).collect()
    }

    /// Pending advisory count (health check).
    #[must_use]
    pub fn pending(&self) -> usize {
        self.ring.len()
    }

    /// Current window counter.
    #[must_use]
    pub fn window(&self) -> u64 {
        self.window
    }

    #[must_use]
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::failure::{DriveTelemetry, FailurePredictor};

    fn suspicion(score_bps: u32) -> Suspicion {
        Suspicion { score_bps, freeze_recommended: score_bps >= 8_000 }
    }

    #[test]
    fn healthy_evidence_emits_nothing() {
        let mut a = Agent::new(AgentConfig::default());
        a.tick();
        a.observe_suspicion(suspicion(200));
        assert_eq!(a.pending(), 0);
        assert!(a.drain().is_empty());
    }

    #[test]
    fn mid_suspicion_logs_only() {
        let mut a = Agent::new(AgentConfig::default());
        a.tick();
        a.observe_suspicion(suspicion(4_500)); // >= 4000 = freeze/2
        let advisories = a.drain();
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].action, AgentAction::LogOnly);
        assert_eq!(advisories[0].evidence, 4_500);
    }

    #[test]
    fn freeze_suspicion_freezes_snapshots() {
        let mut a = Agent::new(AgentConfig::default());
        a.tick();
        a.observe_suspicion(suspicion(9_200));
        let advisories = a.drain();
        assert_eq!(advisories[0].kind, AdvisoryKind::RansomwareSuspicion);
        assert_eq!(advisories[0].action, AgentAction::FreezeSnapshots);
        assert_eq!(advisories[0].evidence, 9_200);
    }

    #[test]
    fn flapping_detector_is_rate_limited() {
        let mut a = Agent::new(AgentConfig::default());
        // Windows 1..=6, all at freeze level: the gap is 5, so
        // emissions land at windows 1 and 6 only.
        for _ in 0..6 {
            a.tick();
            a.observe_suspicion(suspicion(9_000));
        }
        let advisories = a.drain();
        assert_eq!(advisories.len(), 2, "got {:?}", advisories.len());
        assert_eq!(advisories[0].window, 1);
        assert_eq!(advisories[1].window, 6);
    }

    #[test]
    fn drive_risk_actions_escalate_with_band() {
        let p = FailurePredictor::default();
        let mut a = Agent::new(AgentConfig::default());

        let healthy = DriveTelemetry {
            median_latency_us: 500,
            p99_latency_us: 900,
            power_on_hours: 5_000,
            ..DriveTelemetry::default()
        };
        a.tick();
        a.observe_drive(0, &p.assess(&healthy));
        let out = a.drain();
        assert_eq!(out[0].action, AgentAction::LogOnly);

        let degraded = DriveTelemetry {
            realloc_events: 5,
            pending_sectors: 3,
            scrub_repairs: 1,
            median_latency_us: 500,
            p99_latency_us: 900,
            power_on_hours: 40_000,
            ..DriveTelemetry::default()
        };
        a.tick();
        a.observe_drive(0, &p.assess(&degraded));
        let out = a.drain();
        // Default migration_band = Degraded -> plan migration.
        assert_eq!(out[0].action, AgentAction::PlanMigration);

        let failing = DriveTelemetry {
            realloc_events: 40,
            pending_sectors: 20,
            crc_errors: 50,
            scrub_repairs: 5,
            median_latency_us: 200,
            p99_latency_us: 1_200,
            power_on_hours: 60_000,
        };
        a.tick();
        a.observe_drive(0, &p.assess(&failing));
        let out = a.drain();
        assert_eq!(out[0].action, AgentAction::PlanMigration);
        assert!(matches!(out[0].kind, AdvisoryKind::DriveRisk { band: _ }));
    }

    #[test]
    fn per_device_advisories_are_independent() {
        let p = FailurePredictor::default();
        let mut a = Agent::new(AgentConfig::default());
        let t = DriveTelemetry {
            realloc_events: 5,
            pending_sectors: 3,
            scrub_repairs: 1,
            median_latency_us: 500,
            p99_latency_us: 900,
            power_on_hours: 40_000,
            ..DriveTelemetry::default()
        };
        a.tick();
        a.observe_drive(0, &p.assess(&t));
        a.observe_drive(1, &p.assess(&t)); // different device: not rate limited
        assert_eq!(a.drain().len(), 2);
    }

    #[test]
    fn workload_shift_emits_once_per_change() {
        let mut a = Agent::new(AgentConfig::default());
        a.tick();
        a.observe_workload(StreamClass::Db);
        a.tick();
        a.observe_workload(StreamClass::Db); // same class: silent
        assert_eq!(a.drain().len(), 1);
        a.tick();
        a.observe_workload(StreamClass::Vm); // changed: advisory
        let out = a.drain();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].action, AgentAction::RetunePolicies);
        assert!(matches!(out[0].kind, AdvisoryKind::WorkloadShift { .. }));
    }

    #[test]
    fn advisory_ring_is_bounded() {
        let mut cfg = AgentConfig::default();
        cfg.ring_capacity = 4;
        cfg.repeat_window_gap = 0; // allow back-to-back for the test
        let mut a = Agent::new(cfg);
        for w in 0..10 {
            a.tick();
            a.observe_workload(match w % 2 {
                0 => StreamClass::Db,
                _ => StreamClass::Vm,
            });
        }
        assert_eq!(a.pending(), 4);
    }

    #[test]
    fn custom_migration_threshold_failing_only() {
        let p = FailurePredictor::default();
        let mut cfg = AgentConfig::default();
        cfg.migration_band = crate::guardian::failure::RiskBand::Failing;
        let mut a = Agent::new(cfg);
        let degraded = DriveTelemetry {
            realloc_events: 5,
            pending_sectors: 3,
            scrub_repairs: 1,
            median_latency_us: 500,
            p99_latency_us: 900,
            power_on_hours: 40_000,
            ..DriveTelemetry::default()
        };
        a.tick();
        a.observe_drive(0, &p.assess(&degraded));
        let out = a.drain();
        // Migration threshold at Failing: degraded only escalates scrub.
        assert_eq!(out[0].action, AgentAction::EscalateScrub);
    }
}
