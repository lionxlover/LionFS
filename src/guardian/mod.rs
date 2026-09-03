//! # Guardian: the userspace autonomous-operations agent (RFC-004 §7)
//!
//! Guardian is the "small AI" of LionFS -- deliberately *small*: a set
//! of statistical models that run **outside the kernel I/O path** and
//! only ever act out-of-band (advisories, snapshot freezes, migration
//! triggers, policy retunes). The hard rule from RFC-004 §7.0, and the
//! answer to "can the filesystem AI-patch itself": no. The data path
//! stays deterministic and provable (journal TLA+ checked, recovery
//! state machine fault-injected); the agent observes and advises. A
//! filesystem that takes decisions in the I/O path from a model whose
//! behavior you cannot replay bit-for-bit is a filesystem you cannot
//! crash-test -- and crash-testability is the whole product.
//!
//! Four detectors, one policy engine:
//!
//! * [`entropy`] -- the ransomware watcher: rolling Shannon entropy +
//!   rewrite-rate + extension-lure heuristics over write streams.
//! * [`failure`] -- the drive-death predictor: Weibull hazard model
//!   over SMART-style telemetry (realloc events, pending sectors,
//!   CRC errors, latency outliers).
//! * [`workload`] -- the IO stream classifier: online moments ->
//!   sequentiality / IO-size / read-write mix -> DB, LOG, STREAM,
//!   META, VM, VHOST profiles (feeds the compression/tiering retunes).
//! * [`agent`] -- the loop and the advisory bus: detectors emit
//!   evidence, the agent scores it into actions, everything is
//!   logged, every action is reversible.
//!
//! Everything here is userspace: the kernel side only exports the
//! telemetry counters (RFC-004 §8) and accepts policy updates.

pub mod agent;
pub mod entropy;
pub mod failure;
pub mod workload;

pub use agent::{Advisory, AdvisoryKind, Agent, AgentAction, AgentConfig};
pub use entropy::{entropy_bits_per_byte, EntropyWatcher};
pub use failure::{DriveTelemetry, FailurePredictor, RiskBand};
pub use workload::{StreamClass, WorkloadClassifier};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_compose() {
        let mut w = EntropyWatcher::new();
        w.observe(10, 1, 0, b"hello world hello world");
        let _bits = entropy_bits_per_byte(b"aaaa");
        let p = FailurePredictor::default();
        let _ = p.hazard_band(&DriveTelemetry::default());
        let mut c = WorkloadClassifier::new();
        let _ = c.observe(4096, true, 128 * 1024);
        let _ = Agent::new(AgentConfig::default());
    }
}
