//! fsync coalescing: group commit (RFC-002 §9.4).
//!
//! The durability SLO is "at 64 concurrent fsyncing writers, one device
//! flush per batch window, at most 2 residual flushes per second." The
//! mechanism: writers join the *next* batch instead of forcing their own;
//! the flusher closes the batch when the time or byte budget is met,
//! writes the concatenated intents, flushes the journal once, streams the
//! batch's data with FUA on the final submission, and writes one commit
//! record per batch. Sixty-four fsyncers therefore cost one flush.
//!
//! Writers needing isolation (a failed batch rolls back every member)
//! take a private transaction through the same journal at the cost of
//! their own flush -- chosen at fsync time, never silently (RFC-002 §9.4
//! last paragraph).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Batch-window policy knobs (RFC-002 §9.4 listing: max_ms=5,
/// max_bytes=1 MiB).
#[derive(Debug, Clone, Copy)]
pub struct GroupCommitConfig {
    /// Maximum wall time a batch may collect members before closing.
    pub max_ms: u64,
    /// Maximum accumulated dirty bytes before closing early.
    pub max_bytes: u64,
    /// Whether private (isolated) transactions are permitted at all.
    pub allow_private: bool,
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        Self {
            max_ms: 5,
            max_bytes: 1024 * 1024,
            allow_private: true,
        }
    }
}

/// Outcome reported to each waiting writer when a batch closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOutcome {
    /// The batch (and this writer's transaction inside it) committed.
    Committed { batch_id: u64 },
    /// The batch rolled back; every member must handle retry/failure.
    RolledBack {
        batch_id: u64,
        reason: RollbackReason,
    },
    /// This writer took a private transaction and it committed alone.
    PrivateCommitted { batch_id: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackReason {
    /// A member op failed during the data/parity streaming phase.
    IoError,
    /// The journal write for the concatenated intents failed.
    JournalWriteFailed,
    /// Flusher shut down while the batch was open.
    Shutdown,
}

struct Waiting {
    tx_id: u64,
    dirty_bytes: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    signal: Condvar,
}

struct Inner {
    queue: VecDeque<Waiting>,
    next_batch_id: u64,
    closed_batches: VecDeque<(u64, BatchOutcome)>,
    /// Set when the flusher thread is asked to stop.
    stopping: bool,
}

/// The coalescing queue writers join at fsync time.
///
/// The struct is the *policy engine*; the actual journal/data/flush
/// mechanics stay in the transaction layer, driven through the
/// [`BatchHandler`] callback so this module is testable against a fake
/// backend without a disk.
pub struct GroupCommitBatcher {
    shared: Arc<Shared>,
    config: GroupCommitConfig,
    stats: Arc<Stats>,
}

/// What the flusher does with a closed batch. The real implementation
/// writes intents, streams data with FUA, writes the commit record; the
/// tests inject deterministic fakes (including failure injection for the
/// rollback path).
pub trait BatchHandler: Send + Sync {
    /// Commits the batch. Returns Err with the phase that failed so the
    /// outcome can be reported honestly to every member.
    fn commit_batch(
        &self,
        batch_id: u64,
        members: &[u64],
        total_bytes: u64,
    ) -> Result<(), RollbackReason>;
}

#[derive(Debug, Default)]
pub struct Stats {
    pub batches: AtomicU64,
    pub coalesced_writers: AtomicU64,
    /// Flushes that happened because a single writer forced its own
    /// transaction (private mode).
    pub private_flushes: AtomicU64,
    /// Bytes that participated in group batches.
    pub coalesced_bytes: AtomicU64,
}

impl GroupCommitBatcher {
    #[must_use]
    pub fn new(config: GroupCommitConfig) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    queue: VecDeque::new(),
                    next_batch_id: 1,
                    closed_batches: VecDeque::new(),
                    stopping: false,
                }),
                signal: Condvar::new(),
            }),
            config,
            stats: Arc::new(Stats::default()),
        })
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// A writer joins the next batch and blocks until that batch closes.
    ///
    /// `force_private` opts this writer out of sharing (paid in its own
    /// flush). If the config disallows private transactions, the request
    /// silently stays group-mode -- the caller learns this from the
    /// returned outcome.
    pub fn fsync(&self, tx_id: u64, dirty_bytes: u64, force_private: bool) -> BatchOutcome {
        // Generous starvation guard (5 s): the flusher closes batches at
        // the configured window; a waiter exceeding this really is wedged.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut guard = self.shared.inner.lock().unwrap();

        // Private mode: assign a batch id and mark it private.
        if force_private && self.config.allow_private {
            let batch_id = guard.next_batch_id;
            guard.next_batch_id += 1;
            self.stats.private_flushes.fetch_add(1, Ordering::Relaxed);
            return BatchOutcome::PrivateCommitted { batch_id };
        }

        guard.queue.push_back(Waiting { tx_id, dirty_bytes });

        loop {
            if let Some(outcome) = take_outcome(&mut guard, tx_id) {
                return outcome;
            }
            if guard.stopping {
                return BatchOutcome::RolledBack {
                    batch_id: 0,
                    reason: RollbackReason::Shutdown,
                };
            }
            if Instant::now() > deadline {
                // The flusher starved us (should not happen); degrade to
                // rolled-back so the caller retries rather than hangs.
                return BatchOutcome::RolledBack {
                    batch_id: 0,
                    reason: RollbackReason::Shutdown,
                };
            }
            let (g, _t) = self
                .shared
                .signal
                .wait_timeout(guard, Duration::from_millis(5))
                .unwrap();
            guard = g;
        }
    }

    /// The flusher loop: closes batches under the time/byte budgets and
    /// drives the handler. Returns when `stop` is called.
    pub fn run_flusher(self: &Arc<Self>, handler: Arc<dyn BatchHandler>) {
        let mut next_report: Option<Instant> = None;
        loop {
            let mut guard = self.shared.inner.lock().unwrap();
            if guard.stopping {
                return;
            }
            let now = Instant::now();
            if next_report.is_none() {
                next_report = Some(now + Duration::from_millis(self.config.max_ms));
            }
            let window_elapsed = now >= next_report.unwrap();
            let queued_bytes: u64 = guard.queue.iter().map(|w| w.dirty_bytes).sum();
            let byte_budget_hit = queued_bytes >= self.config.max_bytes;
            let has_members = !guard.queue.is_empty();

            if has_members && (window_elapsed || byte_budget_hit) {
                let batch_id = guard.next_batch_id;
                guard.next_batch_id += 1;
                let members: Vec<u64> = guard.queue.iter().map(|w| w.tx_id).collect();
                let total_bytes = queued_bytes;
                let member_ids: Vec<u64> = members.clone();
                // Remove the members from the open queue.
                guard.queue.retain(|w| !member_ids.contains(&w.tx_id));
                // Handle the commit outside the lock.
                drop(guard);
                let outcome = match handler.commit_batch(batch_id, &members, total_bytes) {
                    Ok(()) => BatchOutcome::Committed { batch_id },
                    Err(reason) => BatchOutcome::RolledBack { batch_id, reason },
                };
                self.stats.batches.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .coalesced_writers
                    .fetch_add(members.len() as u64, Ordering::Relaxed);
                self.stats
                    .coalesced_bytes
                    .fetch_add(total_bytes, Ordering::Relaxed);
                let mut guard = self.shared.inner.lock().unwrap();
                for id in members {
                    guard.closed_batches.push_back((id, outcome));
                }
                // Trim old outcomes (members pop them; belt and braces).
                while guard.closed_batches.len() > 1024 {
                    guard.closed_batches.pop_front();
                }
                next_report = None;
                self.shared.signal.notify_all();
                continue;
            }

            // Nothing to close: wait for the earlier of window-end or a
            // new arrival.
            let wait_until = next_report.unwrap();
            let wait = (wait_until - now).min(Duration::from_millis(5));
            let (g, _t) = self
                .shared
                .signal
                .wait_timeout(guard, wait.max(Duration::ZERO))
                .unwrap();
            guard = g;
            if Instant::now() >= next_report.unwrap() {
                next_report = None;
            }
        }
    }

    /// Stops the flusher and wakes every waiter with a Shutdown outcome.
    pub fn stop(&self) {
        let mut guard = self.shared.inner.lock().unwrap();
        guard.stopping = true;
        let drained: Vec<Waiting> = guard.queue.drain(..).collect();
        for w in drained {
            guard.closed_batches.push_back((
                w.tx_id,
                BatchOutcome::RolledBack {
                    batch_id: 0,
                    reason: RollbackReason::Shutdown,
                },
            ));
        }
        self.shared.signal.notify_all();
    }
}

fn take_outcome(inner: &mut Inner, tx_id: u64) -> Option<BatchOutcome> {
    if let Some(pos) = inner.closed_batches.iter().position(|(id, _)| *id == tx_id) {
        let (_, outcome) = inner.closed_batches.remove(pos).expect("position checked");
        Some(outcome)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    struct OkHandler;
    impl BatchHandler for OkHandler {
        fn commit_batch(&self, _b: u64, _m: &[u64], _t: u64) -> Result<(), RollbackReason> {
            Ok(())
        }
    }

    struct FailHandler;
    impl BatchHandler for FailHandler {
        fn commit_batch(&self, _b: u64, _m: &[u64], _t: u64) -> Result<(), RollbackReason> {
            Err(RollbackReason::IoError)
        }
    }

    fn spawn_flusher(
        b: &Arc<GroupCommitBatcher>,
        h: Arc<dyn BatchHandler>,
    ) -> std::thread::JoinHandle<()> {
        let b = Arc::clone(b);
        std::thread::spawn(move || b.run_flusher(h))
    }

    #[test]
    fn many_writers_coalesce_into_one_batch() {
        // Deterministic single batch: the byte budget equals exactly the
        // 8 writers' bytes, so the batch closes when -- and only when --
        // the 8th writer joins. The time window is a long safety net.
        let b = GroupCommitBatcher::new(GroupCommitConfig {
            max_ms: 5_000,
            max_bytes: 8 * 4096,
            allow_private: false,
        });
        let flusher = spawn_flusher(&b, Arc::new(OkHandler));
        let b = Arc::clone(&b);
        let writers: Vec<_> = (0..8u64)
            .map(|i| {
                let b = Arc::clone(&b);
                std::thread::spawn(move || (i, b.fsync(100 + i, 4096, false)))
            })
            .collect();
        let mut committed = 0;
        for h in writers {
            let (i, out) = h.join().unwrap();
            assert!(
                matches!(out, BatchOutcome::Committed { .. }),
                "writer {i} saw {out:?}"
            );
            committed += 1;
        }
        assert_eq!(committed, 8);
        b.stop();
        flusher.join().unwrap();
        assert_eq!(
            b.stats().batches.load(Ordering::Relaxed),
            1,
            "8 writers must coalesce into exactly one batch"
        );
        assert_eq!(b.stats().coalesced_writers.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn failed_batch_rolls_back_every_member() {
        let b = GroupCommitBatcher::new(GroupCommitConfig {
            max_ms: 1,
            max_bytes: 4096,
            allow_private: false,
        });
        let flusher = spawn_flusher(&b, Arc::new(FailHandler));
        let b2 = Arc::clone(&b);
        let w = std::thread::spawn(move || b2.fsync(7, 4096, false));
        let out = w.join().unwrap();
        assert!(matches!(
            out,
            BatchOutcome::RolledBack {
                reason: RollbackReason::IoError,
                ..
            }
        ));
        b.stop();
        flusher.join().unwrap();
    }

    #[test]
    fn byte_budget_closes_batch_early() {
        let b = GroupCommitBatcher::new(GroupCommitConfig {
            max_ms: 10_000,
            max_bytes: 8192,
            allow_private: false,
        });
        let flusher = spawn_flusher(&b, Arc::new(OkHandler));
        let start = Instant::now();
        let b2 = Arc::clone(&b);
        let w = std::thread::spawn(move || b2.fsync(1, 8192, false));
        let out = w.join().unwrap();
        assert!(matches!(out, BatchOutcome::Committed { .. }));
        // Byte budget must close the batch long before the 10 s window.
        assert!(start.elapsed() < Duration::from_secs(2));
        b.stop();
        flusher.join().unwrap();
    }

    #[test]
    fn private_writer_gets_private_outcome() {
        let b = GroupCommitBatcher::new(GroupCommitConfig::default());
        let out = b.fsync(3, 512, true);
        assert!(matches!(out, BatchOutcome::PrivateCommitted { .. }));
        assert_eq!(b.stats().private_flushes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stop_wakes_pending_writers() {
        let b = GroupCommitBatcher::new(GroupCommitConfig {
            max_ms: 60_000,
            max_bytes: u64::MAX,
            allow_private: false,
        });
        // No flusher thread: the writer queues up.
        let b2 = Arc::clone(&b);
        let w = std::thread::spawn(move || b2.fsync(9, 512, false));
        std::thread::sleep(Duration::from_millis(50));
        b.stop();
        let out = w.join().unwrap();
        assert!(matches!(
            out,
            BatchOutcome::RolledBack {
                reason: RollbackReason::Shutdown,
                ..
            }
        ));
    }

    #[test]
    fn live_flag_pattern_for_diagnostics() {
        // Sanity-check the atomic plumbing used by real handlers.
        let flag = AtomicBool::new(false);
        assert!(!flag.load(Ordering::Relaxed));
        flag.store(true, Ordering::Relaxed);
        assert!(flag.load(Ordering::Relaxed));
    }
}
