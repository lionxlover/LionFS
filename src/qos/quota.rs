//! Per-namespace quotas (RFC-004 §4.3): space and inode accounting with
//! hard/soft limits and grace periods.
//!
//! The table is consulted on the **allocation** path, not the submission
//! path — a rate limit can defer an IO, but a quota decision must be
//! made exactly once, where bytes change hands. Soft limits are
//! advisory (returned, logged, exported); hard limits are refusals.
//!
//! Grace periods are evaluated against a caller-supplied clock, keeping
//! the whole module deterministic under the simulator and the property
//! tests. Storage policy (where the table lives on disk: it is an
//! ordinary HAMT subtree) is out of scope here — see RFC-004 §4.5.

use std::collections::HashMap;

/// Which limit tripped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LimitKind {
    /// Soft space limit: exceeded, but inside the grace window.
    SoftSpace,
    /// Hard space limit: refusal.
    HardSpace,
    /// Soft inode limit.
    SoftInode,
    /// Hard inode limit: refusal.
    HardInode,
}

/// One namespace's quota envelope. `None` = unlimited on that axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuotaSpec {
    pub soft_space: Option<u64>,
    pub hard_space: Option<u64>,
    pub soft_inodes: Option<u64>,
    pub hard_inodes: Option<u64>,
    /// Grace window (ns) once a soft limit trips before it escalates to
    /// a hard refusal.
    pub grace_ns: u64,
}

impl QuotaSpec {
    /// Validates the envelope: soft <= hard on both axes, and a hard
    /// limit of zero is rejected (that is a ban; express it as
    /// `hard = Some(1)` if you truly mean "empty namespace").
    #[must_use]
    pub fn validate(&self) -> bool {
        let axes_ok = |soft: Option<u64>, hard: Option<u64>| match (soft, hard) {
            (Some(s), Some(h)) => s <= h && h > 0,
            (Some(_), None) => true,
            (None, Some(h)) => h > 0,
            (None, None) => true,
        };
        axes_ok(self.soft_space, self.hard_space) && axes_ok(self.soft_inodes, self.hard_inodes)
    }
}

/// Live usage for one namespace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NamespaceUsage {
    pub used_bytes: u64,
    pub used_inodes: u64,
    /// When the soft-space limit tripped (clock ns), if it has.
    pub soft_space_since: Option<u64>,
    /// When the soft-inode limit tripped.
    pub soft_inodes_since: Option<u64>,
}

/// The quota verdict returned to the allocator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QuotaDecision {
    /// Go ahead.
    Allow,
    /// Allowed, but flagged: soft limit newly or still exceeded.
    AllowWarn(LimitKind),
    /// Refused: hard limit, or soft limit past grace.
    Deny(LimitKind),
}

/// A note of *why* a denial happened, for the health API (RFC-004 §8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenialRecord {
    pub namespace: u32,
    pub kind: LimitKind,
    pub at_ns: u64,
    pub requested_bytes: u64,
    pub used_bytes: u64,
    pub hard: Option<u64>,
}

/// The table itself: namespace id -> (spec, usage).
///
/// Updates are explicit (`charge`/`release`) so the transaction layer
/// can replay them; the table itself keeps no journal.
#[derive(Default, Debug)]
pub struct QuotaTable {
    specs: HashMap<u32, (QuotaSpec, NamespaceUsage)>,
    denials: Vec<DenialRecord>,
}

impl QuotaTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs or replaces a namespace's envelope. Invalid specs are
    /// rejected (`false`) rather than silently clamped.
    pub fn set_spec(&mut self, namespace: u32, spec: QuotaSpec) -> bool {
        if !spec.validate() {
            return false;
        }
        let entry = self
            .specs
            .entry(namespace)
            .or_insert((spec, NamespaceUsage::default()));
        entry.0 = spec;
        // A new (larger) envelope may clear an outstanding soft trip.
        let usage = &mut entry.1;
        if let Some(soft) = spec.soft_space {
            if usage.used_bytes <= soft {
                usage.soft_space_since = None;
            }
        } else {
            usage.soft_space_since = None;
        }
        if let Some(soft) = spec.soft_inodes {
            if usage.used_inodes <= soft {
                usage.soft_inodes_since = None;
            }
        } else {
            usage.soft_inodes_since = None;
        }
        true
    }

    /// Read-only view (monitoring tools).
    #[must_use]
    pub fn get(&self, namespace: u32) -> Option<(QuotaSpec, NamespaceUsage)> {
        self.specs.get(&namespace).copied()
    }

    /// Evaluates a prospective charge of `bytes` (+`inodes`) at `now_ns`
    /// **without** applying it. Pure; the allocator calls this before
    /// reserving, then [`QuotaTable::charge`] to commit.
    #[must_use]
    pub fn evaluate(&self, namespace: u32, now_ns: u64, bytes: u64, inodes: u64) -> QuotaDecision {
        let Some((spec, usage)) = self.specs.get(&namespace) else {
            return QuotaDecision::Allow; // unmanaged namespace
        };

        // Hard space.
        if let Some(hard) = spec.hard_space {
            if usage.used_bytes.saturating_add(bytes) > hard {
                return QuotaDecision::Deny(LimitKind::HardSpace);
            }
        }
        // Hard inodes.
        if let Some(hard) = spec.hard_inodes {
            if usage.used_inodes.saturating_add(inodes) > hard {
                return QuotaDecision::Deny(LimitKind::HardInode);
            }
        }
        // Soft space: warn, or escalate past grace.
        if let Some(soft) = spec.soft_space {
            if usage.used_bytes.saturating_add(bytes) > soft {
                let since = usage.soft_space_since.unwrap_or(now_ns);
                if now_ns.saturating_sub(since) > spec.grace_ns && spec.grace_ns > 0 {
                    return QuotaDecision::Deny(LimitKind::SoftSpace);
                }
                return QuotaDecision::AllowWarn(LimitKind::SoftSpace);
            }
        }
        // Soft inodes.
        if let Some(soft) = spec.soft_inodes {
            if usage.used_inodes.saturating_add(inodes) > soft {
                let since = usage.soft_inodes_since.unwrap_or(now_ns);
                if now_ns.saturating_sub(since) > spec.grace_ns && spec.grace_ns > 0 {
                    return QuotaDecision::Deny(LimitKind::SoftInode);
                }
                return QuotaDecision::AllowWarn(LimitKind::SoftInode);
            }
        }
        QuotaDecision::Allow
    }

    /// Commits a charge. The caller must have observed `Allow`/
    /// `AllowWarn` from [`QuotaTable::evaluate`] first (this is the
    /// transaction layer's job). Records soft-limit trip timestamps.
    pub fn charge(&mut self, namespace: u32, now_ns: u64, bytes: u64, inodes: u64) {
        if let Some((spec, usage)) = self.specs.get_mut(&namespace) {
            usage.used_bytes = usage.used_bytes.saturating_add(bytes);
            usage.used_inodes = usage.used_inodes.saturating_add(inodes);
            if let Some(soft) = spec.soft_space {
                if usage.used_bytes > soft && usage.soft_space_since.is_none() {
                    usage.soft_space_since = Some(now_ns);
                }
            }
            if let Some(soft) = spec.soft_inodes {
                if usage.used_inodes > soft && usage.soft_inodes_since.is_none() {
                    usage.soft_inodes_since = Some(now_ns);
                }
            }
        }
    }

    /// Releases usage (truncate, unlink, snapshot-shared extents going
    /// zero-ref). Clamps at zero; never fails.
    pub fn release(&mut self, namespace: u32, bytes: u64, inodes: u64) {
        if let Some((spec, usage)) = self.specs.get_mut(&namespace) {
            usage.used_bytes = usage.used_bytes.saturating_sub(bytes);
            usage.used_inodes = usage.used_inodes.saturating_sub(inodes);
            if let Some(soft) = spec.soft_space {
                if usage.used_bytes <= soft {
                    usage.soft_space_since = None;
                }
            }
            if let Some(soft) = spec.soft_inodes {
                if usage.used_inodes <= soft {
                    usage.soft_inodes_since = None;
                }
            }
        }
    }

    /// Records a denial (called by the allocator when `Deny` comes
    /// back). Bounded: the most recent 1024 denials are kept.
    pub fn record_denial(&mut self, namespace: u32, kind: LimitKind, now_ns: u64, requested: u64) {
        let (used_bytes, hard) = self
            .specs
            .get(&namespace)
            .map(|(s, u)| (u.used_bytes, s.hard_space))
            .unwrap_or((0, None));
        self.denials.push(DenialRecord {
            namespace,
            kind,
            at_ns: now_ns,
            requested_bytes: requested,
            used_bytes,
            hard,
        });
        if self.denials.len() > 1024 {
            let drop = self.denials.len() - 1024;
            self.denials.drain(0..drop);
        }
    }

    /// Recent denials, oldest first.
    #[must_use]
    pub fn denials(&self) -> &[DenialRecord] {
        &self.denials
    }

    /// Namespaces under management.
    #[must_use]
    pub fn namespaces(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.specs.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000_000_000;
    const GB: u64 = 1 << 30;

    fn spec() -> QuotaSpec {
        QuotaSpec {
            soft_space: Some(10 * GB),
            hard_space: Some(11 * GB),
            soft_inodes: Some(1000),
            hard_inodes: Some(1200),
            grace_ns: 30 * MIN,
        }
    }

    #[test]
    fn invalid_specs_are_rejected() {
        let mut bad = spec();
        bad.soft_space = Some(12 * GB); // soft > hard
        assert!(!bad.validate());
        let mut bad2 = spec();
        bad2.hard_space = Some(0); // zero hard = ban, rejected
        assert!(!bad2.validate());
        assert!(spec().validate());
        assert!(QuotaSpec::default().validate()); // unlimited
    }

    #[test]
    fn hard_limits_refuse() {
        let mut t = QuotaTable::new();
        assert!(t.set_spec(7, spec()));
        t.charge(7, 0, 10 * GB, 0);
        // 10 GB used, 1 GB headroom: 2 GB request refused.
        assert_eq!(
            t.evaluate(7, 0, 2 * GB, 0),
            QuotaDecision::Deny(LimitKind::HardSpace)
        );
        t.record_denial(7, LimitKind::HardSpace, 5, 2 * GB);
        assert_eq!(t.denials().len(), 1);
        assert_eq!(t.denials()[0].namespace, 7);
    }

    #[test]
    fn soft_limit_warns_then_escalates_after_grace() {
        let mut t = QuotaTable::new();
        assert!(t.set_spec(7, spec()));
        // 9 GB used; +1.5 GB crosses soft (10 GB) but under hard (11 GB).
        t.charge(7, 0, 9 * GB, 0);
        let now = 100;
        assert_eq!(
            t.evaluate(7, now, GB + (GB / 2), 0),
            QuotaDecision::AllowWarn(LimitKind::SoftSpace)
        );
        // Commit the charge; the soft trip timestamp is recorded.
        t.charge(7, now, GB + (GB / 2), 0);
        let usage = t.get(7).expect("managed").1;
        assert_eq!(usage.soft_space_since, Some(now));
        // Inside the grace window: still only a warning.
        assert_eq!(
            t.evaluate(7, now + MIN, 1, 0),
            QuotaDecision::AllowWarn(LimitKind::SoftSpace)
        );
        // Past grace: escalation to refusal.
        assert_eq!(
            t.evaluate(7, now + 31 * MIN, 1, 0),
            QuotaDecision::Deny(LimitKind::SoftSpace)
        );
    }

    #[test]
    fn inode_limits_mirror_space_limits() {
        let mut t = QuotaTable::new();
        assert!(t.set_spec(9, spec()));
        t.charge(9, 0, 0, 1150);
        assert_eq!(
            t.evaluate(9, 0, 0, 100),
            QuotaDecision::Deny(LimitKind::HardInode)
        );
        t.charge(9, 0, 0, 0); // no-op
        t.release(9, 0, 50);
        assert_eq!(
            t.evaluate(9, 0, 0, 100),
            QuotaDecision::AllowWarn(LimitKind::SoftInode)
        );
    }

    #[test]
    fn release_clears_soft_trip() {
        let mut t = QuotaTable::new();
        assert!(t.set_spec(7, spec()));
        t.charge(7, 10, 10 * GB + 1, 0);
        assert!(t.get(7).expect("managed").1.soft_space_since.is_some());
        t.release(7, 2 * GB, 0);
        let usage = t.get(7).expect("managed").1;
        assert!(usage.soft_space_since.is_none());
        assert_eq!(usage.used_bytes, 8 * GB + 1);
    }

    #[test]
    fn unmanaged_namespace_is_unlimited() {
        let t = QuotaTable::new();
        assert_eq!(t.evaluate(404, 0, u64::MAX / 2, u64::MAX / 2), QuotaDecision::Allow);
    }

    #[test]
    fn raising_soft_clears_trip() {
        let mut t = QuotaTable::new();
        assert!(t.set_spec(7, spec()));
        t.charge(7, 5, 10 * GB + 1, 0);
        assert!(t.get(7).expect("managed").1.soft_space_since.is_some());
        let mut bigger = spec();
        bigger.soft_space = Some(12 * GB);
        bigger.hard_space = Some(13 * GB); // keep soft <= hard
        assert!(t.set_spec(7, bigger));
        assert!(t.get(7).expect("managed").1.soft_space_since.is_none());
    }

    #[test]
    fn denial_ring_is_bounded() {
        let mut t = QuotaTable::new();
        assert!(t.set_spec(1, spec()));
        for i in 0..2000u64 {
            t.record_denial(1, LimitKind::HardSpace, i, 4096);
        }
        assert_eq!(t.denials().len(), 1024);
        assert_eq!(t.denials()[0].at_ns, 976); // oldest survivors
    }

    #[test]
    fn namespaces_sorted_for_admin_tools() {
        let mut t = QuotaTable::new();
        for ns in [30, 10, 20] {
            assert!(t.set_spec(ns, QuotaSpec::default()));
        }
        assert_eq!(t.namespaces(), vec![10, 20, 30]);
    }
}
