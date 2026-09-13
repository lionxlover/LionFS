//! Punch-through escape hatch (RFC-002 §7.1).
//!
//! The worst 1.x behavior: a random 4 KiB write into compressed data
//! costs a full 128 KiB decompress-splice-recompress. The 2.0 answer:
//! when the policy engine observes a **third** RMW against the same
//! cluster, the cluster is transparently decompressed into raw extents,
//! its ClusterTree entry is retired, and subsequent random writes hit
//! the plain extent path. Write amplification on the transition is paid
//! once instead of unboundedly. The reverse direction exists too:
//! clusters that go cold and unmodified for a scrub cycle are
//! re-compressed into the cold tier during idle windows.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// RMW hits against a cluster before the escape triggers.
pub const PUNCH_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchThroughDecision {
    /// Stay compressed: the RMW tax is still cheaper than the one-time
    /// decompression.
    StayCompressed,
    /// Punch through: decompress to raw extents, retire the ClusterTree
    /// entry. The caller executes the transition inside a normal
    /// transaction (crash mid-repair heals into old or new, never
    /// neither).
    PunchThrough,
}

/// Tracks per-cluster RMW counts and emits the decision.
#[derive(Debug, Default)]
pub struct PunchThroughTracker {
    rmw_counts: Mutex<HashMap<(u64, u64), u32>>,
    punches: AtomicU64,
    /// Cold-cycle observations (cluster key -> unmodified scrub cycles).
    cold_cycles: Mutex<HashMap<(u64, u64), u32>>,
    recompressions: AtomicU64,
}

/// Scrub cycles of quiescence before a cluster is re-compressed cold.
pub const RECOMPRESS_AFTER_CYCLES: u32 = 2;

impl PunchThroughTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one read-modify-write against (inode, cluster) and
    /// decides.
    pub fn note_rmw(&self, inode: u64, cluster: u64) -> PunchThroughDecision {
        let key = (inode, cluster);
        let mut counts = self.rmw_counts.lock().expect("rmw counts lock");
        let c = counts.entry(key).or_insert(0);
        *c += 1;
        if *c >= PUNCH_THRESHOLD {
            counts.remove(&key);
            drop(counts);
            self.punches.fetch_add(1, Ordering::Relaxed);
            // A punched cluster is no longer cold-cycle-eligible either.
            self.cold_cycles.lock().expect("cold lock").remove(&key);
            PunchThroughDecision::PunchThrough
        } else {
            PunchThroughDecision::StayCompressed
        }
    }

    /// Records that (inode, cluster) went one full scrub cycle unmodified.
    /// Returns true when it should be re-compressed into the cold tier.
    pub fn note_quiescent_cycle(&self, inode: u64, cluster: u64) -> bool {
        let key = (inode, cluster);
        // Unconsumed RMW activity resets quiescence. The marker is then
        // consumed: one observed activity resets once, not forever.
        if self
            .rmw_counts
            .lock()
            .expect("rmw counts lock")
            .remove(&key)
            .is_some()
        {
            self.cold_cycles.lock().expect("cold lock").remove(&key);
            return false;
        }
        let mut cold = self.cold_cycles.lock().expect("cold lock");
        let c = cold.entry(key).or_insert(0);
        *c += 1;
        if *c >= RECOMPRESS_AFTER_CYCLES {
            cold.remove(&key);
            drop(cold);
            self.recompressions.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Clears tracking for a retired/removed cluster.
    pub fn forget(&self, inode: u64, cluster: u64) {
        let key = (inode, cluster);
        self.rmw_counts
            .lock()
            .expect("rmw counts lock")
            .remove(&key);
        self.cold_cycles.lock().expect("cold lock").remove(&key);
    }

    pub fn stats(&self) -> (u64, u64) {
        (
            self.punches.load(Ordering::Relaxed),
            self.recompressions.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_rmws_stay_compressed_third_punches() {
        let t = PunchThroughTracker::new();
        assert_eq!(t.note_rmw(1, 0), PunchThroughDecision::StayCompressed);
        assert_eq!(t.note_rmw(1, 0), PunchThroughDecision::StayCompressed);
        assert_eq!(t.note_rmw(1, 0), PunchThroughDecision::PunchThrough);
        assert_eq!(t.stats().0, 1);
        // After a punch the counter is gone: a fourth RMW (against the
        // now-raw extents, if the transition has not happened yet)
        // starts fresh rather than immediately re-punching.
        assert_eq!(t.note_rmw(1, 0), PunchThroughDecision::StayCompressed);
    }

    #[test]
    fn clusters_are_tracked_independently() {
        let t = PunchThroughTracker::new();
        t.note_rmw(1, 0);
        t.note_rmw(1, 0);
        t.note_rmw(1, 1); // different cluster
        assert_eq!(t.note_rmw(1, 0), PunchThroughDecision::PunchThrough);
        assert_eq!(t.note_rmw(1, 1), PunchThroughDecision::StayCompressed);
        // And across inodes.
        t.note_rmw(2, 0);
        t.note_rmw(2, 0);
        assert_eq!(t.note_rmw(2, 0), PunchThroughDecision::PunchThrough);
        assert_eq!(t.stats().0, 2);
    }

    #[test]
    fn quiescent_cycles_trigger_recompression() {
        let t = PunchThroughTracker::new();
        // First full cycle: quiescent but below the threshold.
        assert!(!t.note_quiescent_cycle(1, 5));
        // A different cluster is tracked independently.
        assert!(!t.note_quiescent_cycle(1, 6));
        // Second full cycle on cluster 5 triggers the cold re-compress.
        assert!(
            t.note_quiescent_cycle(1, 5),
            "second full cycle must trigger"
        );
        assert_eq!(t.stats().1, 1);
        // The trigger consumes the counter: the next cycle starts over.
        assert!(!t.note_quiescent_cycle(1, 5));
    }

    #[test]
    fn rmw_activity_resets_quiescence() {
        let t = PunchThroughTracker::new();
        // One quiescent cycle...
        assert!(!t.note_quiescent_cycle(1, 9));
        // ...then an RMW mid-life restarts the clock (the next quiescent
        // call observes the RMW counter and resets).
        t.note_rmw(1, 9);
        assert!(!t.note_quiescent_cycle(1, 9));
        // A FULL two cycles are then needed again before the trigger.
        assert!(!t.note_quiescent_cycle(1, 9));
        assert!(
            t.note_quiescent_cycle(1, 9),
            "second post-reset cycle must trigger"
        );
    }

    #[test]
    fn forget_clears_both_kinds_of_state() {
        let t = PunchThroughTracker::new();
        t.note_rmw(3, 0);
        t.note_quiescent_cycle(3, 0);
        t.forget(3, 0);
        // Fresh state: single RMW does not punch.
        assert_eq!(t.note_rmw(3, 0), PunchThroughDecision::StayCompressed);
    }
}
