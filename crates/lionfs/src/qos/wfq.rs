//! Weighted fair queuing in virtual time (RFC-004 §4.4).
//!
//! The group-commit batch picker faces N queues (one per tenant or per
//! [`crate::qos::IoClass`] slot). Unfairness under naive round-robin is
//! a measured problem (RFC-004 §4.0): a 4 KiB tenant and a 1 MiB tenant
//! alternating receive 4 KiB : 1 MiB of service. WFQ fixes that by
//! advancing a single *virtual clock* by `cost / weight` per served
//! request and always serving the queue whose *virtual finish time* is
//! earliest -- so a heavy request costs its queue proportionally more
//! future silence.
//!
//! This implementation is generic over the queue count (`const N`), and
//! is a pure *decision function*: the caller owns the queues and the
//! data in them; the scheduler only answers "which queue, given these
//! pending head costs". Semantics of [`WfqScheduler::set_pending`]:
//! declaring a cost for a queue that already has a pending head is
//! idempotent (the head keeps its original finish time) -- finish times
//! are computed once at declaration, the classical WFQ rule, so a
//! caller cannot launder cost by re-declaring. All arithmetic is
//! integer; the deterministic simulator and production share it
//! bit-for-bit.

/// Weighted fair queuing decision state for `N` queues.
#[derive(Clone, Debug)]
pub struct WfqScheduler<const N: usize> {
    weights: [u64; N],
    /// Virtual finish time of each queue's head request. A queue with
    /// nothing pending has `None` and is never picked.
    finish: [Option<u64>; N],
    /// The declared cost of each pending head (for accounting).
    pending_cost: [u64; N],
    /// Accumulated virtual time; monotonically non-decreasing.
    virtual_time: u64,
    /// Total requests served per queue (observability).
    served: [u64; N],
    /// Total cost units served per queue (observability).
    served_cost: [u64; N],
}

impl<const N: usize> WfqScheduler<N> {
    /// Builds a scheduler with per-queue weights. Weight 0 is invalid
    /// (a queue that never pays); the constructor saturates it to 1
    /// rather than failing, so a bad policy file can't wedge the engine.
    #[must_use]
    pub fn new(weights: [u64; N]) -> Self {
        let mut w = weights;
        for weight in &mut w {
            if *weight == 0 {
                *weight = 1;
            }
        }
        Self {
            weights: w,
            finish: [None; N],
            pending_cost: [0; N],
            virtual_time: 0,
            served: [0; N],
            served_cost: [0; N],
        }
    }

    /// Declares queue `q`'s pending head cost. Cost units are
    /// caller-defined (bytes by default). `0` is treated as `1` so a
    /// zero-length op still pays virtual time. Idempotent while a head
    /// is pending: the finish time is computed at first declaration
    /// only (classical WFQ; prevents cost laundering by re-declaring).
    pub fn set_pending(&mut self, q: usize, cost: u64) {
        debug_assert!(q < N);
        if self.finish[q].is_some() {
            return;
        }
        let cost = cost.max(1);
        self.pending_cost[q] = cost;
        // Finish time: when this head would complete if served now.
        self.finish[q] = Some(self.virtual_time + cost / self.weights[q]);
    }

    /// Clears queue `q`'s pending head (drained by other means:
    /// cancellation, error path).
    pub fn clear(&mut self, q: usize) {
        debug_assert!(q < N);
        self.finish[q] = None;
        self.pending_cost[q] = 0;
    }

    /// Picks the queue whose head finishes earliest in virtual time
    /// (lowest queue index breaks ties, deterministically); advances
    /// virtual time to that finish; consumes the head and accounts the
    /// service. Returns `None` when no queue has pending work.
    pub fn pick(&mut self) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut best_f = u64::MAX;
        for (q, f) in self.finish.iter().enumerate() {
            if let Some(f) = f {
                if *f < best_f {
                    best_f = *f;
                    best = Some(q);
                }
            }
        }
        let q = best?;
        self.finish[q] = None;
        self.virtual_time = self.virtual_time.max(best_f);
        self.served[q] += 1;
        self.served_cost[q] = self.served_cost[q].saturating_add(self.pending_cost[q]);
        self.pending_cost[q] = 0;
        Some(q)
    }

    /// Whether any queue has pending work.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.finish.iter().any(Option::is_some)
    }

    /// Current virtual time (diagnostics).
    #[must_use]
    pub fn virtual_time(&self) -> u64 {
        self.virtual_time
    }

    /// Per-queue service counters: (requests, cost units).
    #[must_use]
    pub fn stats(&self) -> ([u64; N], [u64; N]) {
        (self.served, self.served_cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_weights_saturate_to_one() {
        let s = WfqScheduler::<3>::new([0, 5, 0]);
        assert_eq!(s.weights, [1, 5, 1]);
    }

    #[test]
    fn empty_scheduler_picks_nothing() {
        let mut s = WfqScheduler::<2>::new([1, 1]);
        assert!(s.pick().is_none());
        assert!(!s.has_pending());
    }

    #[test]
    fn equal_weights_alternate_fairly() {
        let mut s = WfqScheduler::<2>::new([1, 1]);
        for round in 0..10 {
            s.set_pending(0, 4096);
            s.set_pending(1, 4096);
            // Both declared; q0 wins the first tie; q1's head survives
            // with its ORIGINAL finish time, so the next pick is q1's.
            let first = s.pick().expect("work pending");
            let second = s.pick().expect("work pending");
            assert_ne!(first, second, "round {round}");
        }
        let (served, _) = s.stats();
        assert_eq!(served, [10, 10]);
    }

    #[test]
    fn heavy_requests_cost_their_queue_more_silence() {
        // Queue 0 issues one 64 KiB request; queue 1 streams 4 KiB
        // requests. With equal weights, queue 0's single request is
        // served only once ~16 x 4 KiB of virtual time has elapsed --
        // the classic WFQ amortization of heavy senders.
        let mut s = WfqScheduler::<2>::new([1, 1]);
        s.set_pending(0, 64 * 1024); // declared once, stays pending
        for _ in 0..16 {
            s.set_pending(1, 4096);
            s.pick();
        }
        let (served, _) = s.stats();
        // 16 rounds: 15 go to queue 1 (finish 4K, 8K, ... 60K all under
        // queue 0's 64K), round 16 ties at 64K and picks queue 0.
        assert_eq!(served, [1, 15]);
        assert!(s.has_pending()); // queue 1's 16th request still pending
    }

    #[test]
    fn weights_divide_service_proportionally() {
        // Weights 1:3, identical 100-unit requests: q1 pays 33 vt per
        // service, q0 pays 100 -> service ratio ~3:1 over 30 rounds.
        let mut s = WfqScheduler::<2>::new([1, 3]);
        for _ in 0..30 {
            s.set_pending(0, 100);
            s.set_pending(1, 100);
            s.pick();
        }
        let (served, _) = s.stats();
        let ratio = served[1] as f64 / served[0].max(1) as f64;
        assert!((2.6..3.5).contains(&ratio), "ratio {ratio} (served {served:?})");
    }

    #[test]
    fn redeclaring_pending_head_is_idempotent() {
        let mut s = WfqScheduler::<2>::new([1, 1]);
        s.set_pending(0, 4096);
        s.set_pending(0, 1); // re-declare with tiny cost: ignored
        let before = s.virtual_time();
        let q = s.pick().expect("pending");
        assert_eq!(q, 0);
        assert_eq!(s.virtual_time(), before + 4096);
        let (_, cost) = s.stats();
        assert_eq!(cost[0], 4096); // accounted at first declaration
    }

    #[test]
    fn virtual_time_never_regresses() {
        let mut s = WfqScheduler::<2>::new([1, 1]);
        s.set_pending(0, 1_000_000);
        s.pick();
        let t1 = s.virtual_time();
        s.set_pending(1, 1); // finish = vt + 1
        s.pick();
        assert!(s.virtual_time() >= t1);
    }

    #[test]
    fn cleared_queue_is_not_picked() {
        let mut s = WfqScheduler::<2>::new([1, 1]);
        s.set_pending(0, 4096);
        s.clear(0);
        assert!(s.pick().is_none());
    }

    #[test]
    fn zero_cost_still_pays_one_unit() {
        let mut s = WfqScheduler::<1>::new([1]);
        s.set_pending(0, 0);
        assert_eq!(s.pick(), Some(0));
        assert_eq!(s.virtual_time(), 1);
        let (served, cost) = s.stats();
        assert_eq!(served[0], 1);
        assert_eq!(cost[0], 1);
    }
}
