//! # QoS admission into the shard dispatcher; WFQ into group commit
//! (RFC-004 §4, Phase 8 wiring)
//!
//! Two seams, one per half of the QoS contract:
//!
//! * [`QosShardGate`] — the *admission* seam. Sits at the front of
//!   each per-core shard's submission queue. For every submitted op
//!   it answers one question -- *may this op enter the engine now?* --
//!   by consulting, in order: the namespace quota (space, checked
//!   at allocation time by contract, but evaluated here for early
//!   rejection), then the class's dual token bucket (throughput).
//!   Realtime ops whose bucket is momentarily empty are admitted
//!   anyway and counted as overruns: RFC-004 §4.1's "never throttled
//!   below its guarantee" means the bucket *meters* RT, it does not
//!   block it. Bulk and BestEffort are strict.
//! * [`GroupCommitPicker`] — the *ordering* seam. Group commit wakes,
//!   finds N queues (one per class, or per tenant) with pending
//!   heads, and must pick a batch. Naive round-robin lets a 1 MiB
//!   tenant starve a 4 KiB tenant's latency; the picker wraps
//!   [`WfqScheduler`] so batch order follows virtual finish time and
//!   a heavy request buys its queue proportional future silence.
//!
//! Both are pure-step objects: the engine supplies `now_ns`, nothing
//! inside reads a clock, and the deterministic simulator drives the
//! identical decisions bit-for-bit (that shared arithmetic is what
//! the crash simulator's QoS assertions rely on).
//!
//! ## Tuned defaults (the ③ tuning pass, RFC-004 §4.5)
//!
//! The 3.0.0 release shipped one undifferentiated "default" bucket.
//! The tuned profile differentiates by class, because measured bulk
//! and interactive traffic have different burst tolerance:
//!
//! $$\begin{aligned}
//! r_{\text{RT}} &= 16\ \mathrm{GiB/s} & b_{\text{RT}} &= 1\ \mathrm{GiB} \\
//! r_{\text{BE}} &= 4\ \mathrm{GiB/s}  & b_{\text{BE}} &= 256\ \mathrm{MiB} \\
//! r_{\text{bulk}} &= 1\ \mathrm{GiB/s} & b_{\text{bulk}} &= 64\ \mathrm{MiB}
//! \end{aligned}$$
//!
//! with WFQ weights $w = (8, 4, 1)$ for (RT, BE, bulk): a bulk byte
//! costs its queue 8x the virtual time of a realtime byte, so under
//! sustained saturation the service split converges to
//!
//! $$\frac{S_{\text{RT}}}{S_{\text{bulk}}} \to \frac{w_{\text{RT}}}{w_{\text{bulk}}} = 8$$
//!
//! -- the ratio holds *by construction* in virtual time, regardless
//! of arrival pattern.

use crate::qos::classes::{IoClass, IoLevel, TokenBucket};
use crate::qos::quota::{LimitKind, QuotaDecision, QuotaTable};
use crate::qos::wfq::WfqScheduler;

/// Tuned per-level throughput profile (RFC-004 §4.5).
///
/// Index order matches `IoLevel`'s `tag()`: Realtime, BestEffort,
/// Bulk. Values: (bytes/s, ops/s, burst bytes, burst ops).
pub const TUNED_LEVEL_PROFILE: [(u64, u64, u64, u64); 3] = [
    // Realtime: 16 GiB/s, 1M ops/s, 1 GiB byte headroom. Meters, never
    // blocks (see `Admission::RealtimeOverrun`).
    (16 * (1 << 30), 1_000_000, 1 << 30, 1 << 16),
    // BestEffort: 4 GiB/s, 250K ops/s, 256 MiB headroom -- the tenant
    // default, sized so a 4 GiB/s steady stream rides the burst slot
    // through a 100 ms scheduler hiccup without visible latency.
    (4 * (1 << 30), 250_000, 1 << 28, 1 << 14),
    // Bulk: 1 GiB/s, 100K ops/s, 64 MiB headroom. Scrub/GC/rebalance
    // throughput ceiling: enough to finish a nightly pass, low enough
    // that an aggressive GC sprint cannot saturate a 10 G link alone.
    (1 << 30, 100_000, 1 << 26, 1 << 13),
];

/// Tuned WFQ queue weights for the 3-class picker: (RT, BE, bulk).
/// Service ratio under saturation converges to 8 : 4 : 1.
pub const TUNED_WFQ_WEIGHTS: [u64; 3] = [8, 4, 1];

/// Which tuned profile slot a level maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunedLevel {
    Realtime,
    BestEffort,
    Bulk,
}

impl From<IoLevel> for TunedLevel {
    fn from(level: IoLevel) -> Self {
        match level {
            IoLevel::Realtime => Self::Realtime,
            IoLevel::BestEffort => Self::BestEffort,
            IoLevel::Bulk => Self::Bulk,
        }
    }
}

impl TunedLevel {
    /// Queue index (0..3) in the 3-class wiring picker.
    #[must_use]
    pub fn queue(self) -> usize {
        match self {
            Self::Realtime => 0,
            Self::BestEffort => 1,
            Self::Bulk => 2,
        }
    }
}

/// The shard-dispatcher admission seam.
///
/// Construct with [`QosShardGate::new_tuned`] for the RFC-004 §4.5
/// profile, or `new` with explicit buckets for policy files. The
/// quota table is optional: a pool without multi-tenancy skips the
/// namespace check entirely (denies nothing).
pub struct QosShardGate {
    buckets: [TokenBucket; 3],
    quotas: Option<QuotaTable>,
    /// Ops admitted per queue (RT, BE, bulk) -- the A/B counters.
    admitted: [u64; 3],
    /// RT ops admitted with an empty bucket (the guarantee).
    rt_overruns: u64,
    /// Non-RT ops delayed at admission, by reason.
    delayed: [u64; 3],
    /// Quota denials recorded (the quota table has its own ledger;
    /// this counts decisions seen at the seam).
    quota_denials: u64,
}

impl QosShardGate {
    /// A gate with explicit per-level buckets and no namespace quotas
    /// (single-tenant pool).
    #[must_use]
    pub fn new(buckets: [TokenBucket; 3]) -> Self {
        Self {
            buckets,
            quotas: None,
            admitted: [0; 3],
            rt_overruns: 0,
            delayed: [0; 3],
            quota_denials: 0,
        }
    }

    /// The tuned profile (RFC-004 §4.5), optionally with quotas.
    #[must_use]
    pub fn new_tuned() -> Self {
        let buckets = TUNED_LEVEL_PROFILE.map(|(b_rate, o_rate, b_burst, o_burst)| {
            TokenBucket::new(b_rate, o_rate, b_burst, o_burst)
                .expect("tuned profile rates are nonzero")
        });
        Self {
            buckets,
            quotas: None,
            admitted: [0; 3],
            rt_overruns: 0,
            delayed: [0; 3],
            quota_denials: 0,
        }
    }

    /// Attaches a quota table (multi-tenant pools).
    pub fn with_quotas(mut self, table: QuotaTable) -> Self {
        self.quotas = Some(table);
        self
    }

    /// The admission decision for one submitted op.
    ///
    /// `namespace` is ignored when no quota table is attached.
    /// Charging the quota (`QuotaTable::charge`) stays the
    /// allocation path's job by contract; the gate *evaluates* to
    /// reject early (a 100 GiB write into a 10 GiB-quota namespace
    /// should fail at submit, not at extent allocation).
    pub fn submit(
        &mut self,
        now_ns: u64,
        class: IoClass,
        namespace: u32,
        bytes: u64,
        ops: u64,
    ) -> Admission {
        // 1. Quota evaluation (early rejection only; the allocation
        //    path remains the charging authority).
        if let Some(table) = &self.quotas {
            match table.evaluate(namespace, now_ns, bytes, 0) {
                QuotaDecision::Deny(kind) => {
                    self.quota_denials += 1;
                    return Admission::QuotaDenied(kind);
                }
                QuotaDecision::AllowWarn(_) | QuotaDecision::Allow => {}
            }
        }

        // 2. Token-bucket admission by class level.
        let q = TunedLevel::from(class.level()).queue();
        let bucket = &mut self.buckets[q];
        let charged = bucket.try_charge(now_ns, bytes, ops);
        if charged {
            self.admitted[q] += 1;
            return Admission::Admitted;
        }
        match class.level() {
            IoLevel::Realtime => {
                // The guarantee: RT never blocks on its own meter.
                // The bucket still recorded the failed charge (no
                // tokens consumed), and the overrun counter is the
                // observability hook for capacity planning.
                self.admitted[q] += 1;
                self.rt_overruns += 1;
                Admission::RealtimeOverrun
            }
            _ => {
                self.delayed[q] += 1;
                Admission::Delayed
            }
        }
    }

    /// A/B counters: (admitted, delayed, rt_overruns) per queue, plus
    /// quota denials seen at the seam.
    #[must_use]
    pub fn counters(&self) -> ([u64; 3], [u64; 3], u64, u64) {
        (
            self.admitted,
            self.delayed,
            self.rt_overruns,
            self.quota_denials,
        )
    }

    /// Remaining burst headroom for a level (diagnostics/telemetry).
    #[must_use]
    pub fn headroom(&self, now_ns: u64, level: IoLevel) -> (u64, u64) {
        self.buckets[TunedLevel::from(level).queue()].headroom(now_ns)
    }
}

/// The verdict at the shard gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Tokens charged; the op enters the shard now.
    Admitted,
    /// Realtime admitted with an empty bucket (the §4.1 guarantee).
    /// Metered as an overrun, never delayed.
    RealtimeOverrun,
    /// BestEffort/Bulk with no tokens: the submitter retries after
    /// the refill interval (the seam never blocks internally).
    Delayed,
    /// Namespace quota rejection (hard limit or expired grace).
    QuotaDenied(LimitKind),
}

/// The group-commit batch-picker seam over `N` pending queues.
///
/// The caller (group commit's wake path) declares each queue's
/// pending head cost, then asks for a batch: the picker drains WFQ
/// order up to `max_ops` picks, returning the queue sequence to
/// service. Re-declaring a still-pending queue is idempotent by the
/// WFQ rule (finish times computed at first declaration), so the
/// caller may declare unconditionally on every wake.
pub struct GroupCommitPicker<const N: usize> {
    wfq: WfqScheduler<N>,
    last_batch: Vec<usize>,
}

impl<const N: usize> GroupCommitPicker<N> {
    /// Picker with explicit queue weights.
    #[must_use]
    pub fn new(weights: [u64; N]) -> Self {
        Self {
            wfq: WfqScheduler::new(weights),
            last_batch: Vec::new(),
        }
    }

    /// Declares queue `q`'s pending head cost (bytes by convention).
    /// Idempotent while the head is pending.
    pub fn declare(&mut self, q: usize, cost_bytes: u64) {
        self.wfq.set_pending(q, cost_bytes);
    }

    /// Clears a queue's head (cancelled/errored op left the queue by
    /// other means).
    pub fn clear(&mut self, q: usize) {
        self.wfq.clear(q);
    }

    /// Picks the service order for the next group-commit batch: up to
    /// `max_ops` queue picks in virtual-finish order. An empty result
    /// means nothing is pending.
    pub fn pick_batch(&mut self, max_ops: usize) -> Vec<usize> {
        let mut batch = Vec::with_capacity(max_ops.min(N));
        for _ in 0..max_ops {
            match self.wfq.pick() {
                Some(q) => batch.push(q),
                None => break,
            }
        }
        self.last_batch = batch.clone();
        batch
    }

    /// The previous batch (diagnostics; the A/B log prints this).
    #[must_use]
    pub fn last_batch(&self) -> &[usize] {
        &self.last_batch
    }

    /// Virtual time (diagnostics).
    #[must_use]
    pub fn virtual_time(&self) -> u64 {
        self.wfq.virtual_time()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qos::quota::{NamespaceUsage, QuotaSpec};

    const MS: u64 = 1_000_000;

    #[test]
    fn tuned_gate_admits_under_budget() {
        let mut gate = QosShardGate::new_tuned();
        let be = IoClass::default_class();
        // 1 MiB at t=0 with a full 256 MiB burst: trivially admitted.
        assert_eq!(gate.submit(0, be, 0, 1 << 20, 1), Admission::Admitted);
        let (adm, delay, overrun, quota) = gate.counters();
        assert_eq!(adm, [0, 1, 0]);
        assert_eq!(delay, [0, 0, 0]);
        assert_eq!(overrun, 0);
        assert_eq!(quota, 0);
    }

    #[test]
    fn realtime_guarantee_survives_empty_bucket() {
        // RT bucket with rate 1 byte/s and burst 1: exhausted at once.
        let tight = TokenBucket::new(1, 1, 1, 1).expect("nonzero");
        let mut gate = QosShardGate::new([tight, TokenBucket::unlimited(), TokenBucket::unlimited()]);
        let rt = IoClass::new(IoLevel::Realtime, 0);
        assert_eq!(gate.submit(0, rt, 0, 1, 1), Admission::Admitted);
        // Second charge: no tokens -- but RT is still admitted.
        assert_eq!(gate.submit(1, rt, 0, 1, 1), Admission::RealtimeOverrun);
        let (adm, _, overrun, _) = gate.counters();
        assert_eq!(adm[0], 2);
        assert_eq!(overrun, 1);
    }

    #[test]
    fn bulk_delays_when_empty_and_recovers_after_refill() {
        let tight = TokenBucket::new(1 << 20, 1_000, 1 << 20, 1_000).expect("nonzero"); // 1 MiB/s
        let mut gate = QosShardGate::new([
            TokenBucket::unlimited(),
            TokenBucket::unlimited(),
            tight,
        ]);
        let bulk = IoClass::new(IoLevel::Bulk, 4);
        // Drain the 1 MiB burst in one shot.
        assert_eq!(gate.submit(0, bulk, 0, 1 << 20, 1), Admission::Admitted);
        // Next op immediately: delayed.
        assert_eq!(gate.submit(MS, bulk, 0, 4 << 10, 1), Admission::Delayed);
        // 1 second later: 1 MiB refilled, 4 KiB rides.
        assert_eq!(
            gate.submit(1000 * MS, bulk, 0, 4 << 10, 1),
            Admission::Admitted
        );
        let (_, delay, overrun, _) = gate.counters();
        assert_eq!(delay[2], 1);
        assert_eq!(overrun, 0);
    }

    #[test]
    fn quota_denial_is_early_and_counted() {
        let mut table = QuotaTable::new();
        let spec = QuotaSpec {
            soft_space: Some(8 << 20),
            hard_space: Some(10 << 20),
            soft_inodes: Some(900),
            hard_inodes: Some(1000),
            grace_ns: 60_000_000_000,
        };
        assert!(spec.validate());
        table.set_spec(7, spec);
        let mut gate = QosShardGate::new_tuned().with_quotas(table);
        let be = IoClass::default_class();
        // A 100 MiB op into a 10 MiB hard limit namespace: denied at submit.
        assert_eq!(
            gate.submit(0, be, 7, 100 << 20, 1),
            Admission::QuotaDenied(LimitKind::HardSpace)
        );
        // Namespace 0 has no spec: quota layer allows everything.
        assert_eq!(gate.submit(0, be, 0, 100 << 20, 1), Admission::Admitted);
        let (_, _, _, quota) = gate.counters();
        assert_eq!(quota, 1);
    }

    #[test]
    fn quota_grace_admits_but_counts() {
        // Soft limit exceeded within grace: allowed through the gate.
        let spec = QuotaSpec {
            soft_space: Some(8 << 20),
            hard_space: Some(10 << 20),
            soft_inodes: Some(900),
            hard_inodes: Some(1000),
            grace_ns: 60_000_000_000,
        };
        let mut table = QuotaTable::new();
        table.set_spec(7, spec);
        // Pre-charge usage past the soft limit but under hard.
        table.charge(7, 0, 9 << 20, 0);
        let mut gate = QosShardGate::new_tuned().with_quotas(table);
        let be = IoClass::default_class();
        assert_eq!(gate.submit(0, be, 7, 1 << 20, 1), Admission::Admitted);
    }

    #[test]
    fn picker_orders_by_virtual_finish_time() {
        // RT (w=8) declares a 64 KiB head; bulk (w=1) declares 4 KiB
        // heads. RT's finish: 64K/8 = 8K vt; bulk's: 4K/1 = 4K vt.
        // Bulk goes first despite RT's weight, because WFQ is fair in
        // *virtual time*, not priority-blind.
        let mut picker = GroupCommitPicker::<3>::new(TUNED_WFQ_WEIGHTS);
        picker.declare(0, 64 << 10);
        picker.declare(2, 4 << 10);
        let batch = picker.pick_batch(2);
        assert_eq!(batch, vec![2, 0]);
    }

    #[test]
    fn weighted_ratio_converges_under_saturation() {
        // Both queues saturated with 4 KiB heads, weights 8:4:1 (q0
        // vs q2): service ratio q0/q2 -> 8.
        let mut picker = GroupCommitPicker::<3>::new(TUNED_WFQ_WEIGHTS);
        let mut served = [0usize; 3];
        for _ in 0..8_000 {
            picker.declare(0, 4096);
            picker.declare(2, 4096);
            for q in picker.pick_batch(1) {
                served[q] += 1;
            }
        }
        let ratio = served[0] as f64 / served[2].max(1) as f64;
        assert!((7.4..8.6).contains(&ratio), "ratio {ratio} ({served:?})");
    }

    #[test]
    fn picker_batch_respects_max_ops() {
        let mut picker = GroupCommitPicker::<3>::new([1, 1, 1]);
        picker.declare(0, 1);
        picker.declare(1, 1);
        picker.declare(2, 1);
        assert_eq!(picker.pick_batch(2).len(), 2);
        // One head remains.
        assert!(picker.pick_batch(2).len() >= 1);
    }

    #[test]
    fn declare_is_idempotent_while_pending() {
        let mut picker = GroupCommitPicker::<2>::new([1, 1]);
        picker.declare(0, 4096);
        picker.declare(0, 1 << 20); // re-declare: ignored
        let t0 = picker.virtual_time();
        assert_eq!(picker.pick_batch(1), vec![0]);
        assert_eq!(picker.virtual_time(), t0 + 4096);
    }

    #[test]
    fn tuned_profile_buckets_are_sane() {
        // Every tuned bucket is constructible and nonzero-rate.
        for (b, o, bb, ob) in TUNED_LEVEL_PROFILE {
            assert!(b > 0 && o > 0 && bb > 0 && ob > 0);
            assert!(TokenBucket::new(b, o, bb, ob).is_some());
        }
        // RT's rate dominates bulk's, BE sits between.
        assert!(TUNED_LEVEL_PROFILE[0].0 > TUNED_LEVEL_PROFILE[1].0);
        assert!(TUNED_LEVEL_PROFILE[1].0 > TUNED_LEVEL_PROFILE[2].0);
    }

    #[test]
    fn namespace_usage_type_composes() {
        // The quota types the gate leans on stay constructible.
        let _u = NamespaceUsage::default();
        let _t = QuotaTable::new();
    }
}
