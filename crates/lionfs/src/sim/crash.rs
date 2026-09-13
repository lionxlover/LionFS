//! # The deterministic crash simulator (Phase 8, ②)
//!
//! Runs the full Phase 8 wiring stack -- QoS shard gate, WFQ batch
//! picker, small-write record-log router, GC execution loop,
//! retention daemon, telemetry bridge -- on the simulated clock with
//! seeded workload scripting, and **injects power cuts at
//! deterministic points**: after op #k, at a seeded byte offset in
//! the log image (the tear point). Recovery replays the truncated
//! image and rebuilds the router; the simulator then checks the
//! crash invariants as assertions, not observations:
//!
//! 1. **Prefix property** -- the sequences replayed form a strict
//!    prefix of the sequences the pre-crash ledger recorded. A
//!    mid-batch power cut discards a suffix; it never reorders,
//!    never resurrects.
//! 2. **Overlay convergence** -- the post-crash read-your-write
//!    state equals the pre-crash state restricted to replayed
//!    records. The two views are built by different code paths
//!    (writer-side overlay vs. replay-side rebuild); disagreement is
//!    exactly the class of bug this simulator exists to catch.
//! 3. **Torn-tail discipline** -- a torn tail is reported as `Torn`,
//!    silently discarded, and never mid-record-trusted.
//! 4. **Determinism** -- same seed, same universe: two runs produce
//!    byte-identical reports.
//!
//! The exhaustive crash-point sweep (`CrashSimulator::sweep`) runs
//! the script once per crash op index -- the FoundationDB
//! discipline: every crash point is a test case, not a probability.
//!
//! Scope honesty: the crash model covers the write-path switch (the
//! record log + overlay + checkpoint drain), which is where 3.0's
//! crash-consistency obligations are new. GC relocation and
//! rebalance execute through the same CoW transaction machinery as
//! user writes (RFC-004 §6: "a GC relocation is indistinguishable
//! from a user write to every layer below it"); their crash
//! behavior is the 2.0 journal's, whose replay is covered by the
//! transaction suite. What this simulator adds is the *policy-stack
//! determinism* proof: every decision the daemon threads make is a
//! pure function of (seed, op index), so a bug found in the field
//! reproduces here with two numbers.

use crate::gc::{ReclaimEvent, SegmentStat};
use crate::wiring::gc_loop::{GcExecutionLoop, GcStepReport};
use crate::qos::classes::{IoClass, IoLevel};
use crate::recordlog::{replay, LogEntry, RecordType, TailState};
use crate::sim::{SimClock, SimRng};
use crate::wiring::qos_gate::{GroupCommitPicker, QosShardGate, Admission, TUNED_WFQ_WEIGHTS};
use crate::wiring::retention_daemon::{RetentionDaemon, SnapshotDeleter};
use crate::wiring::small_write::{SmallWriteRouter, WindowPolicy};
use crate::wiring::telemetry_bridge::GuardianTelemetryBridge;
use crate::fs::retention::SnapshotStamp;
use std::io;

/// How the simulator decides when to cut power.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashMode {
    /// No crash: the clean-run control group.
    None,
    /// Crash after op #`at` (script index), tearing the log at a
    /// seeded offset within the un-committed window's bytes.
    AfterOp { at: usize },
}

/// The report one simulation run produces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimReport {
    /// Scripted ops executed before the crash (or all of them).
    pub ops: usize,
    /// Small writes admitted and routed to the record log.
    pub log_writes: u64,
    /// Writes routed to the tree path (too large).
    pub tree_writes: u64,
    /// Group-commit windows closed.
    pub commits: u64,
    /// Checkpoint drains executed.
    pub checkpoints: u64,
    /// QoS admissions delayed at the gate.
    pub qos_delays: u64,
    /// GC rounds executed (with segments relocated).
    pub gc_rounds: u64,
    /// Retention passes executed.
    pub retention_passes: u64,
    /// Snapshots the retention daemon expired.
    pub retention_expired: u64,
    /// Whether a crash was injected.
    pub crashed: bool,
    /// Replayed record count after the crash.
    pub replayed: u64,
    /// How the replay ended (None = clean end of log).
    pub replay_tail: Option<TailState>,
    /// Ledger size at crash time (the pre-crash truth).
    pub ledger_entries: u64,
    /// Invariant verdicts (all must hold; a failure is an assert,
    /// these fields are the audit trail).
    pub prefix_property_held: bool,
    pub overlay_converged: bool,
}

/// One simulator universe.
pub struct CrashSimulator {
    seed: u64,
    clock: SimClock,
    rng: SimRng,
    gate: QosShardGate,
    router: SmallWriteRouter<Vec<u8>>,
    gc: GcExecutionLoop,
    pool: SimPool,
    retention: RetentionDaemon,
    deleter: LedgerDeleter,
    stamps: Vec<SnapshotStamp>,
    bridge: GuardianTelemetryBridge,
    /// The ledger: every record appended pre-crash, in order.
    ledger: Vec<LogEntry>,
    /// Simulated snapshot id counter.
    next_snap: u64,
    /// Telemetry tick counter.
    ticks: u64,
}

/// The simulated pool: three segments aging as the workload churns.
struct SimPool {
    segments: Vec<SegmentStat>,
    total: u64,
    free: u64,
    reclaims: Vec<ReclaimEvent>,
}

impl crate::wiring::gc_loop::RelocationSink for SimPool {
    fn census(&self) -> Vec<SegmentStat> {
        self.segments.clone()
    }
    fn pool_bytes(&self) -> (u64, u64) {
        (self.total, self.free)
    }
    fn evacuate(&mut self, segment_id: u64) -> io::Result<u64> {
        let idx = self
            .segments
            .iter()
            .position(|s| s.segment_id == segment_id)
            .ok_or_else(|| io::Error::other("unknown segment"))?;
        let freeable = self.segments[idx].freeable_bytes();
        self.segments[idx].live_bytes = 0;
        self.segments[idx].total_bytes = 0;
        self.free = self.free.saturating_add(freeable);
        Ok(freeable)
    }
    fn record_reclaim(&mut self, event: ReclaimEvent) {
        self.reclaims.push(event);
    }
}

/// Snapshot deleter that just counts (the sim's snapshot manager).
#[derive(Default)]
struct LedgerDeleter {
    expired: Vec<u64>,
}

impl SnapshotDeleter for LedgerDeleter {
    fn expire_snapshot(&mut self, id: u64) -> io::Result<bool> {
        self.expired.push(id);
        Ok(true)
    }
}

impl CrashSimulator {
    /// A universe from a seed. The workload script, the pool shape,
    /// the retention cadence -- all derived from `seed` alone.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let rng = SimRng::new(seed);
        let pool = SimPool {
            // Three 256 MiB segments, ~40-60% live, aged by the sim's
            // clock origin.
            segments: vec![
                SegmentStat {
                    segment_id: 1,
                    total_bytes: 256 << 20,
                    live_bytes: 100 << 20,
                    age_ns: 5 * 24 * 3_600 * 1_000_000_000,
                    write_cycles: 300,
                },
                SegmentStat {
                    segment_id: 2,
                    total_bytes: 256 << 20,
                    live_bytes: 160 << 20,
                    age_ns: 30 * 24 * 3_600 * 1_000_000_000,
                    write_cycles: 50,
                },
                SegmentStat {
                    segment_id: 3,
                    total_bytes: 256 << 20,
                    live_bytes: 40 << 20,
                    age_ns: 24 * 3_600 * 1_000_000_000,
                    write_cycles: 5_000,
                },
            ],
            total: 3 * (256 << 20),
            // 12% free: the background GC band under tuned watermarks.
            free: (3 * (256 << 20)) / 8,
            reclaims: Vec::new(),
        };
        // Hourly snapshots for the last 100 hours (sim time origin).
        let stamps = (0..100)
            .map(|i| SnapshotStamp {
                id: i as u64 + 1,
                at: 1_000_000 - i * 3_600,
            })
            .collect();
        Self {
            seed,
            clock: SimClock::new(0),
            rng,
            gate: QosShardGate::new_tuned(),
            router: SmallWriteRouter::new(Vec::new(), WindowPolicy::default()),
            gc: GcExecutionLoop::default(),
            pool,
            retention: RetentionDaemon::default(),
            deleter: LedgerDeleter::default(),
            stamps,
            bridge: GuardianTelemetryBridge::new(),
            ledger: Vec::new(),
            next_snap: 101,
            ticks: 0,
        }
    }

    /// Runs the scripted workload for `ops` ops under `mode`,
    /// crashing (and recovering, and verifying) per its instruction.
    pub fn run(mut self, ops: usize, mode: CrashMode) -> SimReport {
        let mut report = SimReport {
            ops,
            ..Default::default()
        };
        let crash_at = match mode {
            CrashMode::None => usize::MAX,
            CrashMode::AfterOp { at } => at,
        };

        let mut file_counter = 0u64;
        for op in 0..ops {
            if op == crash_at {
                report.ops = op;
                return self.crash_and_recover(report);
            }
            let now = self.clock.advance(self.rng.below(50 * 1_000_000)); // <= 50 ms per op
            // Seeded op mix.
            match self.rng.below(16) {
                0..=9 => {
                    // Small write: a new or existing file, 1..1000 bytes.
                    let file_id = if self.rng.below(3) == 0 {
                        file_counter += 1;
                        file_counter
                    } else if file_counter > 0 {
                        1 + self.rng.below(file_counter)
                    } else {
                        file_counter += 1;
                        file_counter
                    };
                    let len = 1 + self.rng.below(1000) as usize;
                    let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                    let class = if self.rng.below(8) == 0 {
                        IoClass::new(IoLevel::Bulk, 4) // 1 in 8: bulk-class writer
                    } else {
                        IoClass::default_class()
                    };
                    let verdict =
                        self.gate.submit(now, class, 0, len as u64, 1);
                    match verdict {
                        Admission::Delayed | Admission::QuotaDenied(_) => {
                            report.qos_delays += 1;
                        }
                        _ => {
                            let kind = if self.rng.below(2) == 0 {
                                RecordType::Create
                            } else {
                                RecordType::Data
                            };
                            let offset = if kind == RecordType::Create { 0 } else { self.rng.below(8) * 16 };
                            if let Ok(entry_seq) = self.router.write(file_id, offset, &payload, kind) {
                                let _ = entry_seq;
                                report.log_writes += 1;
                                self.ledger.push(LogEntry {
                                    kind,
                                    file_id,
                                    offset,
                                    sequence: self.router.sequence() - 1,
                                    payload: payload.clone(),
                                });
                            }
                        }
                    }
                }
                10 => {
                    // Large write: routes to the tree path.
                    let payload = vec![0xAB; 5000];
                    let _ = self.router.write(file_counter, 0, &payload, RecordType::Data);
                    report.tree_writes += 1;
                }
                11 | 12 => {
                    // Window commit: the group-commit durability point.
                    if let Ok(_) = self.router.commit_window() {
                        report.commits += 1;
                    }
                    // WFQ batch pick over the 3-class queues: the
                    // ordering decision the group-commit wake makes.
                    let mut picker = GroupCommitPicker::<3>::new(TUNED_WFQ_WEIGHTS);
                    for q in 0..3 {
                        picker.declare(q, 4096);
                    }
                    let _ = picker.pick_batch(3);
                }
                13 => {
                    // Checkpoint drain: the tree-side write, then the
                    // checkpoint marker.
                    if self.router.drain_decision() == crate::wiring::small_write::DrainDecision::Drain {
                        let mut tree_sink: Vec<LogEntry> = Vec::new();
                        let outcome = self.router.drain(|e| tree_sink.push(e.clone()));
                        if outcome.is_ok() {
                            report.checkpoints += 1;
                            // Ledger truth: drained entries are in the
                            // tree; the log before the checkpoint is
                            // drained. The ledger keeps them (the tree
                            // is durable too); nothing to remove.
                        }
                    }
                }
                14 => {
                    // GC round.
                    let r: GcStepReport = self.gc.step(&mut self.pool, now);
                    if r.planned {
                        report.gc_rounds += 1;
                        self.bridge.ingest_gc(&r);
                    }
                }
                _ => {
                    // Retention pass (rate-limited by the daemon's
                    // hourly interval; the sim's clock advances by
                    // <= 50 ms per op, so a forced pass here would
                    // bypass the interval -- instead the sim relies
                    // on clock passage and the daemon's own gating).
                    let r = self.retention.step(now, &self.stamps, &mut self.deleter, false);
                    if r.ran {
                        report.retention_passes += 1;
                        report.retention_expired += r.expired.len() as u64;
                        self.bridge.ingest_retention(r.expired.len() as u64);
                    }
                    // A snapshot is born (the schedule the daemon
                    // would be feeding).
                    if self.rng.below(4) == 0 {
                        self.stamps.push(SnapshotStamp {
                            id: self.next_snap,
                            at: now / 1_000_000_000,
                        });
                        self.next_snap += 1;
                    }
                }
            }
            // Telemetry tick: the daemon's per-tick ingest.
            if self.rng.below(4) == 0 {
                self.ticks += 1;
                let (_, delayed, _, _) = self.gate.counters();
                self.bridge.ingest_qos([0, 0, 0], delayed);
                self.bridge.observe_window(self.ticks);
            }
        }
        if mode == CrashMode::None {
            // Control run: replay the full image, verify the prefix
            // property trivially holds (nothing crashed).
            let image = self.router.into_inner().clone();
            let (entries, stats) = replay(&image);
            report.replayed = stats.applied;
            report.replay_tail = stats.tail;
            report.ledger_entries = self.ledger.len() as u64;
            report.prefix_property_held = check_prefix(&self.ledger, &entries);
            report.overlay_converged = true; // no crash: overlay IS the truth
            report.crashed = false;
            return report;
        }
        // CrashMode::AfterOp { at } beyond the script length: never
        // reached; treat as a clean run (defensive).
        self.crash_and_recover(report)
    }

    /// Power cut: tears the log image at a seeded offset (within the
    /// last window's bytes), replays, rebuilds, verifies.
    fn crash_and_recover(mut self, mut report: SimReport) -> SimReport {
        report.crashed = true;
        report.ledger_entries = self.ledger.len() as u64;

        // The device image at power-cut time.
        let image = self.router.into_inner();
        // Tear point: seeded, biased toward the tail (most tears hit
        // the un-committed window; a tear inside durable history is
        // the corrupt-header case the healer owns, out of scope here
        // -- we model the power-cut distribution: uniform in the last
        // 25% of the image, or the very end).
        let tear = if image.is_empty() {
            0
        } else {
            let quarter = image.len() / 4;
            let jitter = self.rng.below(u64::try_from(quarter.max(1)).unwrap_or(1) + 1) as usize;
            let cut = image.len().saturating_sub(quarter + jitter);
            cut.min(image.len())
        };
        let torn_image = &image[..tear];

        // Recovery: replay the truncated image.
        let (entries, stats) = replay(torn_image);
        report.replayed = stats.applied;
        report.replay_tail = stats.tail;

        // Invariant 1: prefix property.
        report.prefix_property_held = check_prefix(&self.ledger, &entries);

        // Invariant 2: overlay convergence. The pre-crash overlay
        // (reconstructed from the ledger) must agree with the
        // replay-rebuilt overlay on every replayed file.
        let rebuilt = SmallWriteRouter::from_replay(Vec::new(), WindowPolicy::default(), &entries);
        report.overlay_converged = check_overlay_convergence(&self.ledger, &entries, &rebuilt);

        // Invariant 3: torn-tail discipline is stats.tail's job; a
        // `Corrupt` tail here would mean the tear landed on a full
        // header with a bad magic -- impossible from truncation, so
        // its appearance is a failure worth asserting.
        assert!(
            stats.tail != Some(TailState::Corrupt),
            "truncation cannot produce a Corrupt tail (seed {})",
            self.seed
        );

        // Telemetry survives the crash: the bridge is still
        // scrapeable (the health socket's whole point post-crash).
        let scrape = self.bridge.render();
        assert!(!scrape.is_empty());

        report
    }

    /// The exhaustive crash-point sweep: run the script once per
    /// crash op (op 0..ops), verifying invariants at every point.
    /// Returns per-crash-point verdicts (all must hold; callers
    /// assert).
    pub fn sweep(seed: u64, ops: usize) -> Vec<SimReport> {
        (0..ops)
            .map(|at| CrashSimulator::new(seed).run(ops, CrashMode::AfterOp { at }))
            .collect()
    }
}

/// The prefix property: replayed data records are exactly the
/// ledger's prefix (the ledger tracks data records; control records
/// -- Commit/Checkpoint -- are durability furniture the replay
/// applies but the ledger does not track, so they are filtered
/// before the comparison).
fn check_prefix(ledger: &[LogEntry], entries: &[LogEntry]) -> bool {
    let data: Vec<&LogEntry> = entries
        .iter()
        .filter(|e| !matches!(e.kind, RecordType::Commit | RecordType::Checkpoint))
        .collect();
    if data.len() > ledger.len() {
        return false;
    }
    ledger
        .iter()
        .zip(data.into_iter())
        .all(|(l, e)| l.sequence == e.sequence && l.file_id == e.file_id && l.kind == e.kind && l.payload == e.payload)
}

/// Overlay convergence: for every file the replay rebuilt, the
/// materialized bytes equal the ledger's application of the same
/// (prefix) entries.
fn check_overlay_convergence(
    ledger: &[LogEntry],
    entries: &[LogEntry],
    rebuilt: &SmallWriteRouter<Vec<u8>>,
) -> bool {
    // Files present in the rebuilt overlay (control records carry
    // file_id 0 and no overlay state; excluded).
    let rebuilt_files: Vec<u64> = entries
        .iter()
        .filter(|e| {
            !matches!(e.kind, RecordType::Commit | RecordType::Checkpoint)
        })
        .map(|e| e.file_id)
        .collect::<std::collections::HashSet<u64>>()
        .into_iter()
        .collect();
    for file_id in rebuilt_files {
        // Ledger truth: apply the ledger's entries for this file,
        // restricted to the replayed prefix.
        let max_seq = entries.last().map(|e| e.sequence).unwrap_or(u64::MAX);
        let mut end = 0u64;
        for e in ledger.iter().filter(|e| e.file_id == file_id && e.sequence <= max_seq) {
            end = end.max(e.offset + e.payload.len() as u64);
        }
        let mut truth = vec![0u8; end as usize];
        for e in ledger.iter().filter(|e| e.file_id == file_id && e.sequence <= max_seq) {
            truth[e.offset as usize..(e.offset + e.payload.len() as u64) as usize]
                .copy_from_slice(&e.payload);
        }
        match rebuilt.read_overlay(file_id) {
            Some(bytes) if bytes == truth => continue,
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_run_has_no_crash_and_trivial_prefix() {
        let r = CrashSimulator::new(42).run(200, CrashMode::None);
        assert!(!r.crashed);
        assert!(r.prefix_property_held);
        assert!(r.log_writes > 0);
        assert!(r.ops == 200);
    }

    #[test]
    fn same_seed_same_universe_bit_for_bit() {
        let a = CrashSimulator::new(1234).run(300, CrashMode::AfterOp { at: 173 });
        let b = CrashSimulator::new(1234).run(300, CrashMode::AfterOp { at: 173 });
        assert_eq!(a, b);
    }

    #[test]
    fn crash_invariants_hold_at_every_op_point() {
        // The exhaustive sweep, small script: every crash point.
        let reports = CrashSimulator::sweep(7, 40);
        assert_eq!(reports.len(), 40);
        for (i, r) in reports.iter().enumerate() {
            assert!(r.crashed, "point {i}");
            assert!(r.prefix_property_held, "prefix failed at {i}");
            assert!(r.overlay_converged, "overlay diverged at {i}");
            assert!(
                r.replayed <= r.ledger_entries,
                "replay exceeded ledger at {i}"
            );
        }
    }

    #[test]
    fn crash_invariants_hold_across_seeds() {
        for seed in [0u64, 1, 99, 0xDEAD_BEEF, u64::MAX / 2] {
            let r = CrashSimulator::new(seed).run(250, CrashMode::AfterOp { at: 111 });
            assert!(r.crashed);
            assert!(r.prefix_property_held, "seed {seed}");
            assert!(r.overlay_converged, "seed {seed}");
        }
    }

    #[test]
    fn teardown_bias_hits_the_uncommitted_window() {
        // Across many crashes, torn tails appear (the tear lands
        // mid-record) and are discarded cleanly.
        let mut torn = 0;
        for at in (0..60).step_by(7) {
            let r = CrashSimulator::new(5).run(60, CrashMode::AfterOp { at });
            if r.replay_tail == Some(TailState::Torn) {
                torn += 1;
            }
        }
        assert!(torn > 0, "the tear-point distribution must produce torn tails");
    }

    #[test]
    fn gc_and_retention_execute_in_the_script() {
        let r = CrashSimulator::new(3).run(500, CrashMode::None);
        // 12% free pool: the first GC round evacuates the whole
        // census, the pool goes healthy, later rounds plan nothing.
        assert!(r.gc_rounds > 0, "gc rounds: {}", r.gc_rounds);
        // Retention: the first pass runs (no interval to honor yet);
        // the sim clock advances ~12.5 s over 500 ops, so the hourly
        // interval gates every subsequent attempt -- the daemon's
        // rate limit holding, by design.
        assert_eq!(r.retention_passes, 1, "first pass runs, interval gates the rest");
        assert!(r.retention_expired > 0);
    }

    #[test]
    fn telemetry_bridge_renders_after_crash() {
        // (Asserted inside crash_and_recover; this test exists so a
        // regression reads as a named failure.)
        let _ = CrashSimulator::new(9).run(100, CrashMode::AfterOp { at: 50 });
    }

    #[test]
    fn replay_stats_types_compose() {
        let _ = crate::recordlog::ReplayStats::default();
        let _ = TailState::Torn;
    }
}
