//! # The Phase 8 Wiring (RFC-004 §15): policy layers onto the live engine
//!
//! LionFS 3.0 shipped its policy layers -- QoS, the small-file record
//! journal, the copy-GC planner, GFS retention, online rebalance,
//! Guardian, the Prometheus registry, migration, key envelopes -- as
//! pure, caller-supplied-time objects. That purity is what made them
//! testable and simulation-compatible, but it also meant each one
//! answered "what *should* happen?" while the engine kept doing what
//! it already did. Phase 8 is the wiring: each policy object now sits
//! on the path it was designed to govern, behind a narrow seam that
//! keeps the engine's hot path allocation-light and keeps every
//! decision reproducible in the deterministic simulator.
//!
//! The wiring contract, uniform across every module here:
//!
//! * **The engine owns the thread; the wiring owns the step.** Every
//!   integration point is a `step(now_ns, ...)` style function. The
//!   existing daemon threads (`worker::*`) call it once per wake-up;
//!   the deterministic simulator (`sim`) calls it once per simulated
//!   tick. No `Instant::now()` is taken inside -- the wall clock is
//!   an argument, exactly as in the policy layers.
//! * **Deny-soft, never wedge.** Every admission decision has a
//!   defined fallback when the policy layer is unavailable or the
//!   budget is exhausted; the wiring degrades to the 2.0 behavior
//!   rather than stalling the submitter (RFC-004 §4.4: the fast path
//!   must stay free of unbounded blocking).
//! * **A/B measurable.** Each switch carries a feature-observable
//!   counter pair (wired-path events vs. bypass events) so the RFC-002
//!   §2.4 measurement discipline -- every structural change proven
//!   with a benchmark -- applies to the wiring itself.
//!
//! | Wiring point (this module) | Policy layer | Engine path | RFC-004 |
//! |---|---|---|---|
//! | [`qos_gate`] | Token buckets, quotas, WFQ | shard dispatcher admission; group-commit batch pick | §4 |
//! | [`small_write`] | Record journal + checkpoint policy | the small-write path and read overlay | §5 |
//! | [`gc_loop`] | Cost/benefit planner | scrubber census → relocation execution → allocator | §6 |
//! | [`retention_daemon`] | GFS retention, rebalance planner | snapshot daemon; pool manager | §12 |
//! | [`telemetry_bridge`] | Guardian advisory bus | Prometheus registry & health socket export | §7, §8 |
//! | [`key_flow`] | PBKDF2 + AEAD envelope | mkfs creation, mount unwrap gate, rewrap rotation | §13 |
//! | [`tar_stream`] | Manifest + import plan | the real ustar stream → POSIX write path | §9 |
//!
//! The deterministic crash simulator that exercises this whole stack
//! (including fault injection and replay verification) lives one
//! module over, in [`crate::sim`].

pub mod gc_loop;
pub mod key_flow;
pub mod qos_gate;
pub mod retention_daemon;
pub mod small_write;
pub mod tar_stream;
pub mod telemetry_bridge;

pub use gc_loop::{GcExecutionLoop, GcStepReport, GcQos, RelocationSink};
pub use key_flow::{
    KeyFlowEvent, KeyFlowReport, KeyPromptFlow, MOUNT_ATTEMPT_BUDGET, MountGate,
};
pub use qos_gate::{
    Admission, GroupCommitPicker, QosShardGate, TunedLevel, TUNED_LEVEL_PROFILE,
    TUNED_WFQ_WEIGHTS,
};
pub use retention_daemon::{
    RebalanceDriver, RebalanceStepReport, RetentionDaemon, RetentionStepReport,
    SegmentMover, SnapshotDeleter,
};
pub use small_write::{
    CheckpointOutcome, DrainDecision, SmallWriteRouter, WindowPolicy, WriteRoute,
};
pub use tar_stream::{ImportSink, ImportSummary, TarImportSession, TarParseError};
pub use telemetry_bridge::GuardianTelemetryBridge;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiring_surface_composes() {
        // Every seam is constructible with pure policy objects; no
        // platform state is created by the wiring layer itself.
        let _gate = QosShardGate::new_tuned();
        let mut picker = GroupCommitPicker::<3>::new(TUNED_WFQ_WEIGHTS);
        picker.declare(0, 4096);
        assert_eq!(picker.pick_batch(4), vec![0]);
        let _daemon = RetentionDaemon::default();
        let class = GcExecutionLoop::default();
        let (reclaimed, rounds, evacuations) = class.totals();
        assert_eq!((reclaimed, rounds, evacuations), (0, 0, 0));
        let _bridge = GuardianTelemetryBridge::new();
        let mut router = SmallWriteRouter::new(Vec::new(), WindowPolicy::default());
        assert_eq!(router.route(b"x"), WriteRoute::RecordLog);
    }
}
