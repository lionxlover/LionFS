//! # Guardian and Prometheus onto the sockets (RFC-004 §7-§8, Phase 8
//! wiring)
//!
//! 3.0.0 shipped the Guardian's advisory bus and the Prometheus
//! registry as parallel, unconnected universes. This bridge is the
//! connection: **one object both sockets scrape.**
//!
//! ```text
//! Guardian daemon ──drain()──► Advisory[] ─┐
//!                                            │ ingest_guardian()
//! QoS shard gate ──counters()───────────────┤
//! GC execution loop ──GcStepReport──────────┤ ingest_gc()
//! record-log router ──routes()──────────────┤ ingest_routes()
//! retention daemon ──RetentionStepReport────┤ ingest_retention()
//! rebalance driver ──RebalanceStepReport────┘ ingest_rebalance()
//!                                            │
//!                                            ▼
//!                     GuardianTelemetryBridge (owns the Registry)
//!                            │               │
//!              telemetry socket ◄────────── health socket
//!              (advisory stream,           (Prometheus exposition
//!               JSON lines)                  text, deterministic)
//! ```
//!
//! The registry's scrape output is deterministic (families in name
//! order, series sorted by label), so the simulator's assertion
//! "metric X == N after scripted workload W" is bit-stable. The
//! exposition carries the Phase 8 A/B counters verbatim: the RFC-002
//! §2.4 measurement discipline applied to the wiring itself.
//!
//! ## The advisory-to-metric mapping
//!
//! Each advisory kind maps to a counter series
//! $\texttt{lfs\_guardian\_advisories\_total}\{kind\}$ incremented
//! per emission, plus a gauge holding the latest evidence value
//! (bps for suspicion, risk multiplier $\times 100$ for drives, 0
//! for workload shifts). The window gauge
//! $\texttt{lfs\_guardian\_window}$ lets a scrape detect a stalled
//! agent (window not advancing) -- the one failure mode of an
//! out-of-band daemon that *looks* like silence.

use crate::wiring::gc_loop::GcStepReport;
use crate::guardian::agent::{Advisory, AdvisoryKind, Agent};
use crate::qos::classes::IoLevel;
use crate::telemetry::prometheus::Registry;
use std::rc::Rc;
/// The bridge: owns the registry, ingests every wiring layer's
/// counters, renders for both sockets.
pub struct GuardianTelemetryBridge {
    registry: Registry,
    // Guardian series.
    advisory_counters: [Rc<crate::telemetry::prometheus::Handle>; 3],
    advisory_evidence: [Rc<crate::telemetry::prometheus::Handle>; 3],
    window_gauge: Rc<crate::telemetry::prometheus::Handle>,
    // QoS series (per level).
    qos_admitted: [Rc<crate::telemetry::prometheus::Handle>; 3],
    qos_delayed: [Rc<crate::telemetry::prometheus::Handle>; 3],
    // GC series.
    gc_reclaimed: Rc<crate::telemetry::prometheus::Handle>,
    gc_rounds: Rc<crate::telemetry::prometheus::Handle>,
    // Record-log routes.
    route_log: Rc<crate::telemetry::prometheus::Handle>,
    route_tree: Rc<crate::telemetry::prometheus::Handle>,
    // Retention / rebalance.
    retention_expired: Rc<crate::telemetry::prometheus::Handle>,
    rebalance_moved: Rc<crate::telemetry::prometheus::Handle>,
    // Last advisory stream (the telemetry socket's payload).
    last_advisories: Vec<Advisory>,
}

fn kind_tag(i: usize) -> &'static str {
    match i {
        0 => "ransomware-suspicion",
        1 => "drive-risk",
        _ => "workload-shift",
    }
}

impl Default for GuardianTelemetryBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl GuardianTelemetryBridge {
    /// Builds the bridge with every Phase 8 series pre-registered
    /// (a scrape before any ingest shows all zeros -- a registry that
    /// grows mid-flight is a cardinality leak, prevented here).
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::new();
        let mut advisory_counters = Vec::new();
        let mut advisory_evidence = Vec::new();
        for i in 0..3 {
            advisory_counters.push(Rc::clone(&registry.counter(
                "lfs_guardian_advisories_total",
                "Guardian advisories emitted, by kind",
                vec![("kind".to_owned(), kind_tag(i).to_owned())],
            )));
            advisory_evidence.push(Rc::clone(&registry.gauge(
                "lfs_guardian_advisory_evidence",
                "Latest advisory evidence (bps x100 for suspicion; risk multiplier x100 for drives)",
                vec![("kind".to_owned(), kind_tag(i).to_owned())],
            )));
        }
        let window_gauge = Rc::clone(&registry.gauge(
            "lfs_guardian_window",
            "Current agent window counter (a stalled agent stops advancing this)",
            vec![],
        ));
        let mut qos_admitted = Vec::new();
        let mut qos_delayed = Vec::new();
        for level in [IoLevel::Realtime, IoLevel::BestEffort, IoLevel::Bulk] {
            let tag = match level {
                IoLevel::Realtime => "realtime",
                IoLevel::BestEffort => "besteffort",
                IoLevel::Bulk => "bulk",
            };
            qos_admitted.push(Rc::clone(&registry.counter(
                "lfs_qos_admitted_total",
                "Ops admitted at the shard gate, by class",
                vec![("class".to_owned(), tag.to_owned())],
            )));
            qos_delayed.push(Rc::clone(&registry.counter(
                "lfs_qos_delayed_total",
                "BestEffort/Bulk ops delayed at the shard gate, by class",
                vec![("class".to_owned(), tag.to_owned())],
            )));
        }
        let gc_reclaimed = Rc::clone(&registry.counter(
            "lfs_gc_reclaimed_bytes_total",
            "Bytes reclaimed by the GC execution loop",
            vec![],
        ));
        let gc_rounds = Rc::clone(&registry.counter(
            "lfs_gc_rounds_total",
            "GC planner rounds executed",
            vec![],
        ));
        let route_log = Rc::clone(&registry.counter(
            "lfs_recordlog_routes_total",
            "Small writes routed to the record log",
            vec![("route".to_owned(), "log".to_owned())],
        ));
        let route_tree = Rc::clone(&registry.counter(
            "lfs_recordlog_routes_total",
            "Writes routed to the B-epsilon tree path",
            vec![("route".to_owned(), "tree".to_owned())],
        ));
        let retention_expired = Rc::clone(&registry.counter(
            "lfs_retention_expired_total",
            "Snapshots expired by the retention daemon",
            vec![],
        ));
        let rebalance_moved = Rc::clone(&registry.counter(
            "lfs_rebalance_moved_bytes_total",
            "Bytes moved by the online rebalance driver",
            vec![],
        ));
        Self {
            registry,
            advisory_counters: advisory_counters.try_into().expect("3"),
            advisory_evidence: advisory_evidence.try_into().expect("3"),
            window_gauge,
            qos_admitted: qos_admitted.try_into().expect("3"),
            qos_delayed: qos_delayed.try_into().expect("3"),
            gc_reclaimed,
            gc_rounds,
            route_log,
            route_tree,
            retention_expired,
            rebalance_moved,
            last_advisories: Vec::new(),
        }
    }

    /// Drains the agent and ingests the advisory stream (the
    /// telemetry-socket half of the wiring). The daemon calls this
    /// once per window; the advisory list is retained for the
    /// socket's JSON payload.
    pub fn drain_agent(&mut self, agent: &mut Agent) {
        let advisories = agent.drain();
        self.ingest_guardian(&advisories);
        self.last_advisories = advisories;
    }

    /// Ingests advisories already drained (the simulator's entry
    /// point -- it drives the agent itself).
    pub fn ingest_guardian(&mut self, advisories: &[Advisory]) {
        for a in advisories {
            let idx = match a.kind {
                AdvisoryKind::RansomwareSuspicion => 0,
                AdvisoryKind::DriveRisk { .. } => 1,
                AdvisoryKind::WorkloadShift { .. } => 2,
            };
            self.advisory_counters[idx].inc();
            self.advisory_evidence[idx].set(a.evidence as i64);
        }
    }

    /// Sets the window gauge (stall detection).
    pub fn observe_window(&mut self, window: u64) {
        self.window_gauge.set(window as i64);
    }

    /// Ingests the shard gate's per-tick counter deltas (the daemon
    /// ingests `counters_after - counters_before` each tick; the
    /// bridge accumulates).
    pub fn ingest_qos(
        &mut self,
        admitted: [u64; 3],
        delayed: [u64; 3],
    ) {
        for i in 0..3 {
            self.qos_admitted[i].add(admitted[i]);
            self.qos_delayed[i].add(delayed[i]);
        }
    }

    /// Ingests one GC round report.
    pub fn ingest_gc(&mut self, report: &GcStepReport) {
        self.gc_reclaimed.add(report.bytes_reclaimed);
        if report.planned {
            self.gc_rounds.inc();
        }
    }

    /// Ingests the record-log router's per-tick route deltas (same
    /// delta convention as [`Self::ingest_qos`]).
    pub fn ingest_routes(&mut self, log_routes: u64, tree_routes: u64) {
        self.route_log.add(log_routes);
        self.route_tree.add(tree_routes);
    }

    /// Ingests one retention pass.
    pub fn ingest_retention(&mut self, expired: u64) {
        self.retention_expired.add(expired);
    }

    /// Ingests one rebalance round.
    pub fn ingest_rebalance(&mut self, bytes_moved: u64) {
        self.rebalance_moved.add(bytes_moved);
    }

    /// The health socket's payload: the full Prometheus exposition
    /// document (deterministic ordering).
    #[must_use]
    pub fn render(&mut self) -> String {
        self.registry.render()
    }

    /// The telemetry socket's payload: the retained advisory stream
    /// since the last drain, as `kind action evidence window` lines.
    #[must_use]
    pub fn advisory_stream(&self) -> String {
        let mut out = String::new();
        for a in &self.last_advisories {
            use std::fmt::Write as _;
            let _ = writeln!(
                out,
                "{} {} {} {}",
                a.kind.name(),
                action_tag(a.action),
                a.evidence,
                a.window
            );
        }
        out
    }

    /// Number of registered series (the cardinality bound).
    #[must_use]
    pub fn series_count(&self) -> usize {
        self.registry.series_count()
    }
}

fn action_tag(a: crate::guardian::agent::AgentAction) -> &'static str {
    use crate::guardian::agent::AgentAction as A;
    match a {
        A::FreezeSnapshots => "freeze-snapshots",
        A::EscalateScrub => "escalate-scrub",
        A::PlanMigration => "plan-migration",
        A::RetunePolicies => "retune-policies",
        A::LogOnly => "log-only",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::agent::{Agent, AgentConfig};
    use crate::guardian::entropy::Suspicion;
    use crate::guardian::failure::RiskBand;

    fn advisory(kind: AdvisoryKind, evidence: u64) -> Advisory {
        Advisory {
            kind,
            action: crate::guardian::agent::AgentAction::LogOnly,
            evidence,
            window: 3,
        }
    }

    #[test]
    fn registry_is_fully_registered_at_construction() {
        let bridge = GuardianTelemetryBridge::new();
        // 3 advisory counters + 3 evidence gauges + 1 window + 3 QoS
        // admitted + 3 delayed + 1 GC reclaimed + 1 GC rounds + 2
        // routes + 1 retention + 1 rebalance = 19 (the cardinality
        // bound: a registry that grows mid-flight is a leak).
        assert_eq!(bridge.series_count(), 19);
        // A scrape with zero ingests renders all-zero families.
        let mut bridge = bridge;
        let text = bridge.render();
        assert!(text.contains("lfs_guardian_advisories_total"));
        assert!(text.contains("lfs_gc_reclaimed_bytes_total"));
        assert!(text.contains("lfs_rebalance_moved_bytes_total"));
    }

    #[test]
    fn advisories_map_to_series() {
        let mut bridge = GuardianTelemetryBridge::new();
        bridge.ingest_guardian(&[
            advisory(AdvisoryKind::RansomwareSuspicion, 8_100),
            advisory(AdvisoryKind::DriveRisk { band: RiskBand::Watch }, 250),
        ]);
        bridge.observe_window(3);
        let text = bridge.render();
        assert!(text.contains("lfs_guardian_advisories_total{kind=\"ransomware-suspicion\"} 1"));
        assert!(text.contains("lfs_guardian_advisory_evidence{kind=\"ransomware-suspicion\"} 8100"));
        assert!(text.contains("lfs_guardian_window 3"));
    }

    #[test]
    fn gc_report_ingests_reclaimed_bytes_and_rounds() {
        let mut bridge = GuardianTelemetryBridge::new();
        let report = GcStepReport {
            planned: true,
            segments_relocated: vec![1, 2],
            bytes_reclaimed: 512 << 20,
            estimated_copy_bytes: 1 << 30,
            qos: None,
            error: None,
        };
        bridge.ingest_gc(&report);
        let text = bridge.render();
        assert!(text.contains("lfs_gc_reclaimed_bytes_total 536870912"));
        assert!(text.contains("lfs_gc_rounds_total 1"));
    }

    #[test]
    fn qos_counters_land_per_class() {
        let mut bridge = GuardianTelemetryBridge::new();
        bridge.ingest_qos([10, 20, 30], [0, 2, 5]);
        let text = bridge.render();
        assert!(text.contains("lfs_qos_admitted_total{class=\"realtime\"} 10"));
        assert!(text.contains("lfs_qos_admitted_total{class=\"bulk\"} 30"));
        assert!(text.contains("lfs_qos_delayed_total{class=\"bulk\"} 5"));
    }

    #[test]
    fn drain_agent_feeds_the_stream_and_series() {
        let mut agent = Agent::new(AgentConfig::default());
        // Push a suspicion over the freeze line to force an advisory.
        agent.observe_suspicion(Suspicion { score_bps: 10_000, freeze_recommended: true });
        agent.tick();
        let mut bridge = GuardianTelemetryBridge::new();
        bridge.drain_agent(&mut agent);
        let stream = bridge.advisory_stream();
        assert!(!stream.is_empty());
        let text = bridge.render();
        assert!(text.contains("lfs_guardian_advisories_total{kind=\"ransomware-suspicion\"} 1"));
    }

    #[test]
    fn scrape_output_is_deterministic() {
        let mut b1 = GuardianTelemetryBridge::new();
        let mut b2 = GuardianTelemetryBridge::new();
        for b in [&mut b1, &mut b2] {
            b.ingest_qos([1, 2, 3], [0, 0, 0]);
            b.ingest_gc(&GcStepReport {
                planned: true,
                bytes_reclaimed: 42,
                ..Default::default()
            });
        }
        assert_eq!(b1.render(), b2.render());
    }

    #[test]
    fn risk_band_types_compose() {
        // The failure-model types the bridge leans on stay public.
        let _ = RiskBand::Healthy;
        let _ = IoLevel::Bulk;
    }
}
