//! IO priority classes (RFC-004 §4.1) and the dual token bucket
//! (RFC-004 §4.2).
//!
//! The class model is deliberately iocost/ionice-shaped because admins
//! already know it: three levels (Realtime, BestEffort, Bulk) each
//! subdivided into 8 sub-levels, folded into one `slot()` the shard
//! dispatcher indexes directly. The token bucket is the classic
//! lazy-refill dual (bytes + ops) with an explicit `now_ns` clock so
//! tests and the deterministic simulator drive it identically to
//! production; no `Instant` is taken inside.

/// IO service levels. Order matters: it is the priority order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum IoLevel {
    /// Latency-critical: journal commits, fsync barriers, WAL-style
    /// writers. Never throttled below its guarantee.
    Realtime,
    /// Default tenant workload class.
    BestEffort,
    /// Scrub, GC, rebalance, migration, backup: throughput-optimized
    /// work that yields to everything above it.
    Bulk,
}

impl IoLevel {
    /// Stable on-disk / policy-JSON tag.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::Realtime => 0,
            Self::BestEffort => 1,
            Self::Bulk => 2,
        }
    }

    /// Inverse of [`IoLevel::tag`]; unknown tags are `None`.
    #[must_use]
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Realtime),
            1 => Some(Self::BestEffort),
            2 => Some(Self::Bulk),
            _ => None,
        }
    }
}

/// Sub-levels per class (0 = highest priority within the class).
pub const SUB_LEVELS: u8 = 8;

/// Total scheduler slots (3 classes x 8 sub-levels).
pub const SCHED_SLOT_COUNT: usize = 3 * SUB_LEVELS as usize;

/// A fully-qualified IO class: level + sub-level.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct IoClass {
    level: IoLevel,
    sub: u8,
}

impl IoClass {
    /// Constructs a class; `sub` is masked into `0..8`.
    #[must_use]
    pub fn new(level: IoLevel, sub: u8) -> Self {
        Self {
            level,
            sub: sub % SUB_LEVELS,
        }
    }

    #[must_use]
    pub fn level(self) -> IoLevel {
        self.level
    }

    #[must_use]
    pub fn sub(self) -> u8 {
        self.sub
    }

    /// The flat scheduler slot in `0..24`: level-major, sub-minor. The
    /// shard dispatcher indexes its ready-queue array by this directly.
    #[must_use]
    pub fn slot(self) -> usize {
        (self.level.tag() as usize) * SUB_LEVELS as usize + self.sub as usize
    }

    /// The default class for a pid: Realtime for pid <= 2 (kernel-ish),
    /// Bulk for scrub/GC-marked pids is set explicitly by the caller,
    /// everyone else lands here.
    #[must_use]
    pub fn default_class() -> Self {
        Self::new(IoLevel::BestEffort, 4)
    }
}

/// A dual token bucket: bytes/s and ops/s, each with burst headroom.
///
/// Refill is lazy: `try_charge` computes elapsed time from the
/// caller-supplied `now_ns` and tops both buckets up to their burst
/// caps first, so the bucket costs nothing while idle. All arithmetic
/// is saturating -- a misconfigured 10 G burst cannot overflow.
///
/// This is the *admission* half of QoS (RFC-004 §4.2); it runs before
/// the WfqScheduler picks an ordering.
#[derive(Clone, Debug)]
pub struct TokenBucket {
    rate_bytes_per_s: u64,
    rate_ops_per_s: u64,
    burst_bytes: u64,
    burst_ops: u64,
    tokens_bytes: u64,
    tokens_ops: u64,
    last_refill_ns: u64,
}

impl TokenBucket {
    /// Unlimited (no rate limits); useful for Realtime by default.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            rate_bytes_per_s: u64::MAX,
            rate_ops_per_s: u64::MAX,
            burst_bytes: u64::MAX,
            burst_ops: u64::MAX,
            tokens_bytes: u64::MAX,
            tokens_ops: u64::MAX,
            last_refill_ns: 0,
        }
    }

    /// A bucket with explicit byte/op rates and burst caps. `None` if
    /// either rate is zero (a zero rate is a ban; model that with
    /// [`QuotaTable`] hard limits instead, so it is visible).
    #[must_use]
    pub fn new(rate_bytes_per_s: u64, rate_ops_per_s: u64, burst_bytes: u64, burst_ops: u64) -> Option<Self> {
        if rate_bytes_per_s == 0 || rate_ops_per_s == 0 {
            return None;
        }
        let caps = burst_bytes.max(1);
        let capo = burst_ops.max(1);
        Some(Self {
            rate_bytes_per_s,
            rate_ops_per_s,
            burst_bytes: caps,
            burst_ops: capo,
            tokens_bytes: caps,
            tokens_ops: capo,
            last_refill_ns: 0,
        })
    }

    /// Lazily refills from `last_refill_ns` to `now_ns`.
    fn refill(&mut self, now_ns: u64) {
        if now_ns <= self.last_refill_ns {
            return;
        }
        let elapsed_ns = now_ns - self.last_refill_ns;
        // Whole-seconds and fractional-ns split avoids f64 and keeps the
        // deterministic simulator bit-identical with production.
        let secs = elapsed_ns / 1_000_000_000;
        let frac_ns = elapsed_ns % 1_000_000_000;
        let add_b = self
            .rate_bytes_per_s
            .saturating_mul(secs)
            .saturating_add(self.rate_bytes_per_s.saturating_mul(frac_ns) / 1_000_000_000);
        let add_o = self
            .rate_ops_per_s
            .saturating_mul(secs)
            .saturating_add(self.rate_ops_per_s.saturating_mul(frac_ns) / 1_000_000_000);
        self.tokens_bytes = self.tokens_bytes.saturating_add(add_b).min(self.burst_bytes);
        self.tokens_ops = self.tokens_ops.saturating_add(add_o).min(self.burst_ops);
        self.last_refill_ns = now_ns;
    }

    /// Tries to admit an operation of `bytes` at time `now_ns`.
    /// Returns `true` and consumes tokens, or `false` (unconsumed).
    pub fn try_charge(&mut self, now_ns: u64, bytes: u64, ops: u64) -> bool {
        self.refill(now_ns);
        if self.tokens_bytes >= bytes && self.tokens_ops >= ops {
            self.tokens_bytes -= bytes;
            self.tokens_ops -= ops;
            true
        } else {
            false
        }
    }

    /// Headroom report for observability (RFC-004 §8 exporter).
    #[must_use]
    pub fn headroom(&self, now_ns: u64) -> (u64, u64) {
        let mut probe = self.clone();
        probe.refill(now_ns);
        (probe.tokens_bytes, probe.tokens_ops)
    }

    #[must_use]
    pub fn rate_bytes_per_s(&self) -> u64 {
        self.rate_bytes_per_s
    }

    #[must_use]
    pub fn rate_ops_per_s(&self) -> u64 {
        self.rate_ops_per_s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn slots_are_level_major() {
        let rt = IoClass::new(IoLevel::Realtime, 0);
        let be4 = IoClass::new(IoLevel::BestEffort, 4);
        let bulk7 = IoClass::new(IoLevel::Bulk, 7);
        assert_eq!(rt.slot(), 0);
        assert_eq!(be4.slot(), 12);
        assert_eq!(bulk7.slot(), 23);
        assert!(rt.slot() < be4.slot() && be4.slot() < bulk7.slot());
        // Sub-level masking: 9 wraps to 1.
        assert_eq!(IoClass::new(IoLevel::BestEffort, 9).slot(), 9);
        assert_eq!(SCHED_SLOT_COUNT, 24);
    }

    #[test]
    fn level_tags_roundtrip() {
        for tag in 0..3 {
            assert_eq!(IoLevel::from_tag(tag).map(|l| l.tag()), Some(tag));
        }
        assert!(IoLevel::from_tag(3).is_none());
    }

    #[test]
    fn unlimited_bucket_always_admits() {
        let mut b = TokenBucket::unlimited();
        assert!(b.try_charge(0, u64::MAX / 2, u64::MAX / 2));
        assert!(b.try_charge(1, 4096, 1));
    }

    #[test]
    fn zero_rate_is_rejected() {
        assert!(TokenBucket::new(0, 100, 1000, 10).is_none());
        assert!(TokenBucket::new(100, 0, 1000, 10).is_none());
    }

    #[test]
    fn bucket_charges_and_exhausts() {
        // 10 MiB/s, 1000 ops/s, burst 2 MiB / 200 ops.
        let mut b = TokenBucket::new(10 << 20, 1000, 2 << 20, 200).expect("valid");
        assert!(b.try_charge(0, 1 << 20, 100)); // fits burst
        assert!(b.try_charge(0, 1 << 20, 100)); // exactly exhausts burst
        assert!(!b.try_charge(0, 1, 1)); // nothing left
    }

    #[test]
    fn bucket_refills_over_time() {
        // 1000 ops/s, burst 10.
        let mut b = TokenBucket::new(100 << 20, 1000, 100 << 20, 10).expect("valid");
        assert!(b.try_charge(0, 0, 10));
        assert!(!b.try_charge(0, 0, 1));
        // 500 ms later: 500 tokens accrued, capped by burst=10.
        assert!(b.try_charge(500 * MS, 0, 5));
        assert!(!b.try_charge(500 * MS, 0, 6));
        // A full second later: back to full burst.
        assert!(b.try_charge(1500 * MS, 0, 10));
    }

    #[test]
    fn refill_is_fractional_and_deterministic() {
        // 10 ops/s, burst 10: after 900 ms, 9 tokens.
        let mut b = TokenBucket::new(1 << 20, 10, 1 << 20, 10).expect("valid");
        assert!(b.try_charge(0, 0, 10)); // drain
        let mut probe = b.clone();
        probe.refill(900 * MS);
        assert_eq!(probe.headroom(900 * MS).1, 9);
        // Same answer when asked twice (idempotence).
        assert_eq!(probe.headroom(900 * MS).1, 9);
    }

    #[test]
    fn failed_charge_consumes_nothing() {
        let mut b = TokenBucket::new(1 << 20, 10, 1 << 20, 10).expect("valid");
        assert!(b.try_charge(0, 0, 4));
        assert!(!b.try_charge(0, 0, 10)); // too big, denied
        assert!(b.try_charge(0, 0, 6)); // exactly the rest
    }

    #[test]
    fn burst_cap_saturates() {
        // 1000 ops/s but burst cap 5: an hour of idle still yields 5.
        let mut b = TokenBucket::new(1 << 20, 1000, 1 << 20, 5).expect("valid");
        assert!(b.try_charge(0, 0, 5));
        let (t, _) = b.headroom(3_600_000_000_000);
        assert_eq!(t, 1 << 20);
    }
}
