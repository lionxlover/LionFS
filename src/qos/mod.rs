//! # QoS & Multi-Tenancy (RFC-004 §4)
//!
//! Three independent control points, each a pure policy object the I/O
//! engine consults on the submission path:
//!
//! * [`IoClass`] — priority classes (RFC-004 §4.1). The *latency* knob:
//!   a scheduler slot per (class, level), iocost-style, that the group
//!   commit and shard dispatcher consult before picking a batch.
//! * [`TokenBucket`] — the *throughput* knob: dual token buckets
//!   (bytes/s and ops/s) with burst headroom, refilled lazily against a
//!   caller-supplied clock so it is deterministic under test.
//! * [`QuotaTable`] — the *space* knob: per-namespace usage with
//!   hard/soft limits and grace periods, checked on allocation, not on
//!   submission.
//! * [`WfqScheduler`] — the *fairness* knob: weighted fair queuing in
//!   virtual time across classes, so a bulk tenant cannot starve a
//!   latency-sensitive one even when both are admitted.
//!
//! All four are synchronous, allocation-light, and free of platform
//! state: the engine supplies time, the policy supplies decisions.
//! That split is what keeps the io_uring fast path free of locks
//! beyond the shard's own (RFC-004 §4.4).

pub mod classes;
pub mod quota;
pub mod wfq;

pub use classes::{IoClass, IoLevel, SCHED_SLOT_COUNT};
pub use quota::{LimitKind, NamespaceUsage, QuotaTable};
pub use wfq::WfqScheduler;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_compose() {
        let c = IoClass::new(IoLevel::Realtime, 0);
        assert_eq!(c.slot(), 0);
        let _q = QuotaTable::new();
        let _w = WfqScheduler::<4>::new([1, 2, 4, 8]);
    }
}
