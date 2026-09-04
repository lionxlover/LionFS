//! # The small-write path switch: record journal onto the live path
//! (RFC-004 §5, Phase 8 wiring)
//!
//! The 3.0.0 release shipped [`RecordLog`] as format-and-policy with
//! the tree-drain step unwired. This module is the drain, the overlay,
//! and the route decision -- the whole §5.1 diagram, executed:
//!
//! ```text
//! write(file, payload)
//!     │
//!     ├─ len > SMALL_FILE_MAX ────────────► B-epsilon tree path (2.0)
//!     │
//!     └─ len <= SMALL_FILE_MAX ──► SmallWriteRouter::write
//!                                   RecordLog::append (CRC32C'd)
//!                                   │
//!                     WindowPolicy window full? ──► commit_window()
//!                     (bytes / records / caller's fsync tick)
//!                                   │
//!                     read(file): overlay first, tree on miss
//!                                   │
//!                     DrainDecision::Drain ──► drain entries into the
//!                     tree (caller's sink), mark_checkpoint(seq)
//! ```
//!
//! Crash discipline is inherited from the log itself:
//! write-before-tree, self-describing records, replay stops at the
//! first torn or corrupt tail. The router adds one invariant on top:
//! **the overlay only exposes records the replay would apply** -- a
//! record appended after the last `Commit` is visible to the writer
//! (read-your-write semantics) but is *not* claimed durable; a crash
//! discards exactly that suffix, and the post-crash overlay is
//! rebuilt from replay, so the two can never disagree.
//!
//! The checkpoint policy (RFC-004 §5.2) is the cost side: a log that
//! never drains is a log that replays forever on every mount. The
//! drain trigger is the log's own `checkpoint_due` (byte budget OR
//! record budget OR a chatty control-burst tail), evaluated through
//! [`SmallWriteRouter::drain_decision`]; the drain itself is a
//! caller-supplied closure so the transaction layer can run it inside
//! an ordinary CoW transaction -- below that seam, a GC relocation
//! and a log drain are indistinguishable, by design.
//!
//! ## The window math
//!
//! Group-commit amortization for a window of $n$ records with average
//! payload $\bar{p}$ bytes against device bandwidth $B$ and fixed
//! per-op cost $c$:
//!
//! $$T_{\text{window}}(n) = \frac{n\,\bar{p}}{B} + c \quad\text{vs.}\quad
//!   T_{\text{scattered}}(n) = \frac{n\,\bar{p}}{B} + n\,c$$
//!
//! The window's win is $(n-1)\,c$: at $n = 64$ and a 20 µs NVMe
//! per-op cost, that is 1.26 ms saved per window -- the measured
//! small-file gap the RFC set out to close.

use crate::recordlog::{LogEntry, RecordLog, RecordType, SMALL_FILE_MAX};
use std::collections::BTreeMap;
use std::io;

/// When the group-commit window should flush, and when the log should
/// drain into the tree (RFC-004 §5.1-§5.2).
///
/// The engine ticks the router after every append; the window policy
/// is two budgets, either of which flushes. Time-based flushing stays
/// the engine's job (it owns the clock-driven wake); this is the
/// byte/record side the router can decide alone. Checkpoint budgets
/// feed the log's own `checkpoint_due` verbatim.
#[derive(Clone, Copy, Debug)]
pub struct WindowPolicy {
    /// Bytes buffered in the open window at which it commits.
    pub byte_budget: u64,
    /// Records buffered in the open window at which it commits.
    pub record_budget: u64,
    /// Checkpoint byte budget (drain trigger, RFC-004 §5.2).
    pub checkpoint_bytes: u64,
    /// Checkpoint record budget (drain trigger).
    pub checkpoint_records: u64,
}

impl Default for WindowPolicy {
    fn default() -> Self {
        Self {
            // RFC-004 §5.1's 1 MiB window, byte and record sides.
            byte_budget: 1 << 20,
            record_budget: 256,
            // Drain when the log holds 16 MiB or 4096 live records.
            checkpoint_bytes: 16 << 20,
            checkpoint_records: 4096,
        }
    }
}

/// Which path a write takes (the A/B counters bucket on this).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteRoute {
    /// The record-log fast path (small write).
    RecordLog,
    /// The ordinary B-epsilon tree path (large write).
    Tree,
}

/// What the drain step did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CheckpointOutcome {
    /// Entries drained into the tree.
    pub entries_drained: u64,
    /// Sequence the checkpoint marker advanced through.
    pub checkpoint_through: u64,
}

/// The drain verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DrainDecision {
    /// Log is under both budgets: hold.
    Hold,
    /// Byte or record budget exceeded (or chatty control tail):
    /// drain now.
    Drain,
}

/// One overlay record: the un-committed or un-drained view of a file
/// region, newest-last within its file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OverlayRec {
    kind: RecordType,
    file_id: u64,
    offset: u64,
    payload: Vec<u8>,
    sequence: u64,
    /// Whether a Commit record covers this entry (durability claim).
    durable: bool,
}

/// The live small-write router: one per shard, sink-agnostic.
///
/// `W` is the log's device sink (the group-commit batch buffer in the
/// engine; a `Vec<u8>` in tests and the deterministic simulator).
pub struct SmallWriteRouter<W: io::Write> {
    log: RecordLog<W>,
    policy: WindowPolicy,
    /// Overlay: file_id -> records in append order.
    overlay: BTreeMap<u64, Vec<OverlayRec>>,
    /// Bytes in the open (un-committed) window.
    window_bytes: u64,
    /// Records in the open window.
    window_records: u64,
    /// Route counters (A/B): [record-log, tree].
    routes: [u64; 2],
    /// Commits issued (windows closed).
    commits: u64,
    /// Checkpoints issued (drains completed).
    checkpoints: u64,
}

impl<W: io::Write> SmallWriteRouter<W> {
    /// Router over a fresh (empty) log sink.
    #[must_use]
    pub fn new(sink: W, policy: WindowPolicy) -> Self {
        Self {
            log: RecordLog::new(sink),
            policy,
            overlay: BTreeMap::new(),
            window_bytes: 0,
            window_records: 0,
            routes: [0, 0],
            commits: 0,
            checkpoints: 0,
        }
    }

    /// Router over a log sink, with overlay state rebuilt from a
    /// post-crash replay: the caller replays the recovered image
    /// ([`crate::recordlog::replay`]) and hands the surviving entries
    /// here, so the pre-crash read-your-write view and the post-crash
    /// replay view are the same map.
    #[must_use]
    pub fn from_replay(sink: W, policy: WindowPolicy, entries: &[LogEntry]) -> Self {
        let mut router = Self::new(sink, policy);
        // Durability: everything the replay applied was on the device
        // under a valid CRC; entries after the last Commit marker are
        // applied too (they were on the device), but flagged
        // not-durable so a subsequent window commit can claim them.
        let last_commit = entries
            .iter()
            .rposition(|e| e.kind == RecordType::Commit);
        for (i, e) in entries.iter().enumerate() {
            let durable = matches!(last_commit, Some(c) if i <= c);
            if e.kind == RecordType::Commit || e.kind == RecordType::Checkpoint {
                continue; // control records carry no file state
            }
            router.overlay.entry(e.file_id).or_default().push(OverlayRec {
                kind: e.kind,
                file_id: e.file_id,
                offset: e.offset,
                payload: e.payload.clone(),
                sequence: e.sequence,
                durable,
            });
        }
        router
    }

    /// The route decision and, when the record-log path is taken, the
    /// append itself. `kind` should be `Create` for the first write
    /// of a file's life, `Data` thereafter (the router does not
    /// track file lifetimes; the VFS layer above it does).
    pub fn write(
        &mut self,
        file_id: u64,
        offset: u64,
        payload: &[u8],
        kind: RecordType,
    ) -> io::Result<WriteRoute> {
        if self.route(payload) == WriteRoute::Tree {
            self.routes[1] += 1;
            return Ok(WriteRoute::Tree);
        }
        let seq = self.log.append(kind, file_id, offset, payload)?;
        self.routes[0] += 1;
        self.window_bytes += payload.len() as u64;
        self.window_records += 1;
        self.overlay.entry(file_id).or_default().push(OverlayRec {
            kind,
            file_id,
            offset,
            payload: payload.to_vec(),
            sequence: seq,
            durable: false,
        });
        Ok(WriteRoute::RecordLog)
    }

    /// The pure route decision: small payloads (and only small
    /// payloads) take the log.
    #[must_use]
    pub fn route(&self, payload: &[u8]) -> WriteRoute {
        if payload.len() <= SMALL_FILE_MAX as usize {
            WriteRoute::RecordLog
        } else {
            WriteRoute::Tree
        }
    }

    /// Should the current window commit now? (Byte/record side; the
    /// engine adds the time side at its own tick.)
    #[must_use]
    pub fn window_ready(&self) -> bool {
        self.window_bytes >= self.policy.byte_budget
            || self.window_records >= self.policy.record_budget
    }

    /// Closes the group-commit window: every record so far becomes
    /// durable (the commit marker is the durability point). Returns
    /// the commit record's sequence.
    pub fn commit_window(&mut self) -> io::Result<u64> {
        let seq = self.log.commit()?;
        self.commits += 1;
        self.window_bytes = 0;
        self.window_records = 0;
        // Everything buffered is now covered by a commit.
        for recs in self.overlay.values_mut() {
            for r in recs.iter_mut() {
                r.durable = true;
            }
        }
        Ok(seq)
    }

    /// The drain decision (RFC-004 §5.2): should the log drain into
    /// the tree now? Delegates to the log's `checkpoint_due` with
    /// this policy's budgets.
    #[must_use]
    pub fn drain_decision(&self) -> DrainDecision {
        if self.log.checkpoint_due(self.policy.checkpoint_bytes, self.policy.checkpoint_records)
        {
            DrainDecision::Drain
        } else {
            DrainDecision::Hold
        }
    }

    /// Drains the overlay into the tree: hands every buffered entry,
    /// in global sequence order, to `sink` (the transaction layer's
    /// tree-insert path), then marks the checkpoint through the last
    /// drained sequence. The tree observes the same op order a
    /// post-crash replay would apply.
    pub fn drain<F>(&mut self, mut sink: F) -> io::Result<CheckpointOutcome>
    where
        F: FnMut(&LogEntry),
    {
        let mut flat: Vec<OverlayRec> = self
            .overlay
            .values()
            .flatten()
            .cloned()
            .collect();
        flat.sort_by_key(|r| r.sequence);
        let drained = flat.len() as u64;
        let through = self.log.sequence().saturating_sub(1); // last data seq
        for r in &flat {
            sink(&LogEntry {
                kind: r.kind,
                file_id: r.file_id,
                offset: r.offset,
                sequence: r.sequence,
                payload: r.payload.clone(),
            });
        }
        self.log.mark_checkpoint(through)?;
        self.overlay.clear();
        self.checkpoints += 1;
        Ok(CheckpointOutcome {
            entries_drained: drained,
            checkpoint_through: through,
        })
    }

    /// Read through the overlay: records applied in sequence order
    /// over a zero base (the caller splices tree data beneath when
    /// the file predates the overlay). Returns `None` when the file
    /// has no overlay state -- the caller falls through to the tree,
    /// the ordinary 2.0 read.
    #[must_use]
    pub fn read_overlay(&self, file_id: u64) -> Option<Vec<u8>> {
        let recs = self.overlay.get(&file_id)?;
        if recs.is_empty() {
            return None;
        }
        let mut end = 0u64;
        for r in recs {
            end = end.max(r.offset + r.payload.len() as u64);
        }
        let mut buf = vec![0u8; end as usize];
        for r in recs {
            let start = r.offset as usize;
            let stop = start + r.payload.len();
            if stop <= buf.len() {
                buf[start..stop].copy_from_slice(&r.payload);
            }
        }
        Some(buf)
    }

    /// Bytes in the open (un-committed) window.
    #[must_use]
    pub fn window_bytes(&self) -> u64 {
        self.window_bytes
    }

    /// The log's byte-side checkpoint pressure (diagnostics).
    #[must_use]
    pub fn bytes_since_checkpoint(&self) -> u64 {
        self.log.bytes_since_checkpoint()
    }

    /// The log's record-side checkpoint pressure (diagnostics).
    #[must_use]
    pub fn records_since_checkpoint(&self) -> u64 {
        self.log.records_since_checkpoint()
    }

    /// The current append sequence (diagnostics).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.log.sequence()
    }

    /// Route/commit/checkpoint counters: the A/B measurement surface.
    #[must_use]
    pub fn counters(&self) -> ([u64; 2], u64, u64) {
        (self.routes, self.commits, self.checkpoints)
    }

    /// Number of overlay entries (test/diagnostic).
    #[must_use]
    pub fn overlay_len(&self) -> u64 {
        self.overlay.values().map(|v| v.len() as u64).sum()
    }

    /// Consumes the router, returning the sink. The crash simulator
    /// truncates the sink's tail through this to model power loss
    /// mid-batch, then replays the image.
    pub fn into_inner(self) -> W {
        self.log.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recordlog::{replay, TailState};

    const REC_BYTES: usize = 44; // 40-byte header + 4-byte CRC

    #[test]
    fn small_writes_route_to_the_log_large_to_the_tree() {
        let mut r = SmallWriteRouter::new(Vec::new(), WindowPolicy::default());
        assert_eq!(r.route(&[0u8; 100]), WriteRoute::RecordLog);
        assert_eq!(r.route(&[0u8; SMALL_FILE_MAX as usize]), WriteRoute::RecordLog);
        assert_eq!(r.route(&[0u8; SMALL_FILE_MAX as usize + 1]), WriteRoute::Tree);
    }

    #[test]
    fn overlay_gives_read_your_write() {
        let mut r = SmallWriteRouter::new(Vec::new(), WindowPolicy::default());
        r.write(7, 0, b"hello", RecordType::Create).unwrap();
        assert_eq!(r.read_overlay(7), Some(b"hello".to_vec()));
        // No overlay state for an unseen file: fall through to tree.
        assert_eq!(r.read_overlay(8), None);
    }

    #[test]
    fn later_records_win_within_a_file() {
        let mut r = SmallWriteRouter::new(Vec::new(), WindowPolicy::default());
        r.write(1, 0, b"aaaaa", RecordType::Create).unwrap();
        r.write(1, 2, b"BBB", RecordType::Data).unwrap();
        // Offsets 2..5 overwritten by the later record.
        assert_eq!(r.read_overlay(1), Some(b"aaBBB".to_vec()));
    }

    #[test]
    fn window_flushes_at_record_budget_and_marks_durability() {
        let mut policy = WindowPolicy::default();
        policy.record_budget = 2;
        let mut r = SmallWriteRouter::new(Vec::new(), policy);
        r.write(1, 0, b"one", RecordType::Create).unwrap();
        assert!(!r.window_ready());
        assert_eq!(r.window_bytes(), 3);
        r.write(2, 0, b"two", RecordType::Create).unwrap();
        assert!(r.window_ready());
        let seq = r.commit_window().unwrap();
        assert!(seq >= 2);
        assert_eq!(r.window_bytes(), 0);
        let (routes, commits, _) = r.counters();
        assert_eq!(routes, [2, 0]);
        assert_eq!(commits, 1);
    }

    #[test]
    fn drain_feeds_the_tree_in_sequence_order_and_clears() {
        let mut policy = WindowPolicy::default();
        policy.checkpoint_records = 2;
        let mut r = SmallWriteRouter::new(Vec::new(), policy);
        r.write(1, 0, b"one", RecordType::Create).unwrap();
        assert_eq!(r.drain_decision(), DrainDecision::Hold);
        r.write(2, 0, b"two", RecordType::Create).unwrap();
        assert_eq!(r.drain_decision(), DrainDecision::Drain);

        let mut seen: Vec<(u64, u64, Vec<u8>)> = Vec::new();
        let outcome = r
            .drain(|e| seen.push((e.file_id, e.sequence, e.payload.clone())))
            .unwrap();
        assert_eq!(
            seen,
            vec![
                (1, 0, b"one".to_vec()),
                (2, 1, b"two".to_vec()),
            ]
        );
        assert_eq!(outcome.entries_drained, 2);
        // Overlay cleared: reads now fall through to the tree.
        assert_eq!(r.overlay_len(), 0);
        assert_eq!(r.read_overlay(1), None);
        assert_eq!(r.drain_decision(), DrainDecision::Hold);
    }

    #[test]
    fn crash_discards_uncommitted_tail_replay_rebuilds_overlay() {
        // Two records, one commit, one un-committed record, then a
        // "crash" that tears the tail mid-record.
        let mut policy = WindowPolicy::default();
        policy.record_budget = 100; // no implicit window flush
        let mut r = SmallWriteRouter::new(Vec::new(), policy);
        r.write(1, 0, b"one", RecordType::Create).unwrap();
        r.write(2, 0, b"two", RecordType::Create).unwrap();
        r.commit_window().unwrap();
        r.write(3, 0, b"three", RecordType::Create).unwrap();
        assert_eq!(r.overlay_len(), 3);

        // Power cut: the last record is torn (half its bytes landed).
        let image = r.into_inner();
        let torn = &image[..image.len() - (REC_BYTES + 5) / 2];
        let (entries, stats) = replay(torn);
        assert_eq!(stats.tail, Some(TailState::Torn));
        // Files 1 and 2 replayed; file 3's torn record discarded.
        assert!(entries.iter().any(|e| e.file_id == 1));
        assert!(entries.iter().any(|e| e.file_id == 2));
        assert!(!entries.iter().any(|e| e.file_id == 3));

        // Rebuild: read-your-write state converges with the replay.
        let router2 = SmallWriteRouter::from_replay(Vec::new(), policy, &entries);
        assert_eq!(router2.read_overlay(1), Some(b"one".to_vec()));
        assert_eq!(router2.read_overlay(2), Some(b"two".to_vec()));
        assert_eq!(router2.read_overlay(3), None);
        assert_eq!(stats.applied, 3); // 2 data + the commit marker
    }

    #[test]
    fn counters_expose_the_ab_split() {
        let mut r = SmallWriteRouter::new(Vec::new(), WindowPolicy::default());
        r.write(1, 0, b"small", RecordType::Create).unwrap();
        r.write(1, 0, &[0u8; 8192], RecordType::Data).unwrap(); // tree
        let (routes, _, _) = r.counters();
        assert_eq!(routes, [1, 1]);
    }

    #[test]
    fn chatty_control_tail_triggers_drain() {
        // Many commits with no data between them (chatty small-file
        // workloads) is the log's third drain trigger.
        let mut r = SmallWriteRouter::new(Vec::new(), WindowPolicy::default());
        r.write(1, 0, b"x", RecordType::Create).unwrap();
        assert_eq!(r.drain_decision(), DrainDecision::Hold);
        r.commit_window().unwrap();
        // A commit as the last record with 2+ records since
        // checkpoint: chatty tail -> drain.
        assert_eq!(r.drain_decision(), DrainDecision::Drain);
    }
}
