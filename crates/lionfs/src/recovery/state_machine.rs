//! The five-state mount recovery machine (RFC-002 §5.4).
//!
//! > 1. PROBE: read SB0/SB1/SB2, choose highest generation with valid
//! >    CRC32C.
//! > 2. REPLAY: walk intent log from journal_seq; roll forward committed,
//! >    discard open.
//! > 3. CHECKPOINT: swap roots, rewrite superblocks, reset the journal.
//! > 4. RECONCILE: merge bad-blocks and ZNS zone tables with
//! >    device-reported state.
//! > 5. WRITABLE: open rings, start shards, begin accepting submissions.
//!
//! Every transition has one obligation: make the smallest change that
//! restores a provably consistent view, then get out of the way. The
//! whole path is exercised by the failure-injection harness (RFC-002
//! §9.5); a crash at any instruction boundary must land in a state this
//! machine converges.
//!
//! This module models the machine as a generic, testable state machine
//! over a [`RecoveryBackend`] trait (the 1.x `Disk` + journal replay
//! implements it; tests drive it with fakes that fault at named points).

use std::fmt;

/// The recovery states, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecoveryState {
    /// Reading all three superblocks, choosing the best.
    Probe,
    /// Walking the intent log: rolling forward committed transactions,
    /// discarding open ones.
    Replay,
    /// Swapping tree roots, rewriting superblocks, resetting the journal.
    Checkpoint,
    /// Merging bad-blocks and zone tables with device reports.
    Reconcile,
    /// Rings open, shards started, submissions accepted.
    Writable,
}

impl RecoveryState {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Probe => "PROBE",
            Self::Replay => "REPLAY",
            Self::Checkpoint => "CHECKPOINT",
            Self::Reconcile => "RECONCILE",
            Self::Writable => "WRITABLE",
        }
    }
}

/// What one state transition did (the audit log the health bus keeps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionRecord {
    /// Superblock copies read; the winner's generation.
    ProbedSuperblock { generation: u64, chosen_copy: u8 },
    /// Journal walk results: committed transactions rolled forward,
    /// open ones discarded.
    ReplayedJournal {
        rolled_forward: u64,
        discarded_open: u64,
    },
    /// Roots swapped under a new generation; journal reset.
    Checkpointed { new_generation: u64 },
    /// Zone/bad-block tables reconciled with device reports.
    Reconciled {
        bad_blocks_merged: u64,
        zones_updated: u64,
    },
    /// Engine accepting submissions.
    Writable,
}

impl fmt::Display for TransitionRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProbedSuperblock {
                generation,
                chosen_copy,
            } => {
                write!(f, "probe: chose SB{chosen_copy} generation {generation}")
            }
            Self::ReplayedJournal {
                rolled_forward,
                discarded_open,
            } => {
                write!(
                    f,
                    "replay: {rolled_forward} rolled forward, {discarded_open} open discarded"
                )
            }
            Self::Checkpointed { new_generation } => {
                write!(
                    f,
                    "checkpoint: roots swapped at generation {new_generation}"
                )
            }
            Self::Reconciled {
                bad_blocks_merged,
                zones_updated,
            } => {
                write!(
                    f,
                    "reconcile: {bad_blocks_merged} bad blocks, {zones_updated} zones updated"
                )
            }
            Self::Writable => write!(f, "writable"),
        }
    }
}

/// The surface the state machine drives. The real implementation wraps
/// `Disk` + `RecoveryManager` + the zone table; tests implement fakes
/// that can fail at any named point (the §9.5 kill points).
pub trait RecoveryBackend {
    /// Superblock candidates: (copy index, generation, crc_valid).
    fn probe_superblocks(&mut self) -> std::io::Result<Vec<(u8, u64, bool)>>;
    /// Replays the intent journal; returns (rolled_forward, discarded).
    fn replay_journal(&mut self, from_generation: u64) -> std::io::Result<(u64, u64)>;
    /// Checkpoints: swaps roots, rewrites SBs, resets the journal.
    fn checkpoint(&mut self, at_generation: u64) -> std::io::Result<()>;
    /// Reconciles zone and bad-block tables with device reports.
    fn reconcile(&mut self) -> std::io::Result<(u64, u64)>;
    /// Opens rings and starts shards.
    fn go_writable(&mut self) -> std::io::Result<()>;
}

/// The mount-time recovery machine.
pub struct RecoveryStateMachine<'a> {
    backend: &'a mut dyn RecoveryBackend,
    state: RecoveryState,
    audit: Vec<TransitionRecord>,
    /// Highest generation observed in PROBE (drives REPLAY/CHECKPOINT).
    chosen_generation: u64,
}

impl<'a> RecoveryStateMachine<'a> {
    #[must_use]
    pub fn new(backend: &'a mut dyn RecoveryBackend) -> Self {
        Self {
            backend,
            state: RecoveryState::Probe,
            audit: Vec::new(),
            chosen_generation: 0,
        }
    }

    #[must_use]
    pub fn state(&self) -> RecoveryState {
        self.state
    }

    #[must_use]
    pub fn audit(&self) -> &[TransitionRecord] {
        &self.audit
    }

    /// Runs the machine to WRITABLE. Each state's step is idempotent
    /// where the underlying operations are (replay is; checkpoint bumps
    /// generation), which is what makes a crash-during-recovery re-run
    /// converge.
    pub fn run_to_writable(&mut self) -> std::io::Result<()> {
        while self.state != RecoveryState::Writable {
            self.step()?;
        }
        Ok(())
    }

    /// Advances exactly one state.
    pub fn step(&mut self) -> std::io::Result<()> {
        match self.state {
            RecoveryState::Probe => {
                let candidates = self.backend.probe_superblocks()?;
                let Some(&(copy, generation, true)) = candidates
                    .iter()
                    .filter(|(_, _, valid)| *valid)
                    .max_by_key(|(_, gen, _)| *gen)
                else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "no CRC-valid superblock among SB0/SB1/SB2",
                    ));
                };
                self.chosen_generation = generation;
                self.audit.push(TransitionRecord::ProbedSuperblock {
                    generation,
                    chosen_copy: copy,
                });
                self.state = RecoveryState::Replay;
            }
            RecoveryState::Replay => {
                let (rolled_forward, discarded_open) =
                    self.backend.replay_journal(self.chosen_generation)?;
                self.audit.push(TransitionRecord::ReplayedJournal {
                    rolled_forward,
                    discarded_open,
                });
                self.state = RecoveryState::Checkpoint;
            }
            RecoveryState::Checkpoint => {
                let new_generation = self.chosen_generation.wrapping_add(1);
                self.backend.checkpoint(new_generation)?;
                self.chosen_generation = new_generation;
                self.audit
                    .push(TransitionRecord::Checkpointed { new_generation });
                self.state = RecoveryState::Reconcile;
            }
            RecoveryState::Reconcile => {
                let (bad_blocks_merged, zones_updated) = self.backend.reconcile()?;
                self.audit.push(TransitionRecord::Reconciled {
                    bad_blocks_merged,
                    zones_updated,
                });
                self.state = RecoveryState::Writable;
                self.backend.go_writable()?;
                self.audit.push(TransitionRecord::Writable);
            }
            RecoveryState::Writable => {
                // Terminal: no-op (step on a terminal state).
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A scripted fake backend: returns canned results and records the
    /// call order, optionally failing at a named point (the §9.5 kill
    /// points).
    struct FakeBackend {
        calls: RefCell<Vec<&'static str>>,
        superblocks: Vec<(u8, u64, bool)>,
        replay: (u64, u64),
        reconcile: (u64, u64),
        fail_at: Option<&'static str>,
    }

    impl FakeBackend {
        fn healthy() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                superblocks: vec![(0, 10, true), (1, 12, true), (2, 9, true)],
                replay: (3, 1),
                reconcile: (2, 7),
                fail_at: None,
            }
        }

        fn record(&self, name: &'static str) -> std::io::Result<()> {
            if self.fail_at == Some(name) {
                return Err(std::io::Error::other(format!("injected fault at {name}")));
            }
            self.calls.borrow_mut().push(name);
            Ok(())
        }
    }

    impl RecoveryBackend for FakeBackend {
        fn probe_superblocks(&mut self) -> std::io::Result<Vec<(u8, u64, bool)>> {
            self.record("probe")?;
            Ok(self.superblocks.clone())
        }
        fn replay_journal(&mut self, _g: u64) -> std::io::Result<(u64, u64)> {
            self.record("replay")?;
            Ok(self.replay)
        }
        fn checkpoint(&mut self, _g: u64) -> std::io::Result<()> {
            self.record("checkpoint")
        }
        fn reconcile(&mut self) -> std::io::Result<(u64, u64)> {
            self.record("reconcile")?;
            Ok(self.reconcile)
        }
        fn go_writable(&mut self) -> std::io::Result<()> {
            self.record("writable")
        }
    }

    #[test]
    fn healthy_run_walks_all_states_in_order() {
        let mut backend = FakeBackend::healthy();
        let mut sm = RecoveryStateMachine::new(&mut backend);
        sm.run_to_writable().unwrap();
        assert_eq!(sm.state(), RecoveryState::Writable);
        // The audit tells the whole story.
        assert_eq!(sm.audit().len(), 5);
        drop(sm);
        let calls = backend.calls.borrow().clone();
        assert_eq!(
            calls,
            vec!["probe", "replay", "checkpoint", "reconcile", "writable"]
        );
    }

    #[test]
    fn probe_picks_highest_generation_valid_copy() {
        let mut backend = FakeBackend::healthy();
        // SB1 has the highest valid generation (12).
        let mut sm = RecoveryStateMachine::new(&mut backend);
        sm.step().unwrap();
        match sm.audit()[0] {
            TransitionRecord::ProbedSuperblock {
                generation,
                chosen_copy,
            } => {
                assert_eq!(generation, 12);
                assert_eq!(chosen_copy, 1);
            }
            ref other => panic!("unexpected first record: {other:?}"),
        }
    }

    #[test]
    fn corrupt_superblocks_fail_mount_explicitly() {
        let mut backend = FakeBackend::healthy();
        backend.superblocks = vec![(0, 10, false), (1, 12, false), (2, 9, false)];
        let mut sm = RecoveryStateMachine::new(&mut backend);
        let err = sm.run_to_writable().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_valid_lower_generation_beats_a_corrupt_higher_one() {
        let mut backend = FakeBackend::healthy();
        // SB1 (gen 12) is corrupt: SB0 (gen 10) wins.
        backend.superblocks = vec![(0, 10, true), (1, 12, false), (2, 9, true)];
        let mut sm = RecoveryStateMachine::new(&mut backend);
        sm.step().unwrap();
        match sm.audit()[0] {
            TransitionRecord::ProbedSuperblock {
                generation,
                chosen_copy,
            } => {
                assert_eq!(generation, 10);
                assert_eq!(chosen_copy, 0);
            }
            ref other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn step_is_granular() {
        let mut backend = FakeBackend::healthy();
        let mut sm = RecoveryStateMachine::new(&mut backend);
        assert_eq!(sm.state(), RecoveryState::Probe);
        sm.step().unwrap();
        assert_eq!(sm.state(), RecoveryState::Replay);
        sm.step().unwrap();
        assert_eq!(sm.state(), RecoveryState::Checkpoint);
        sm.step().unwrap();
        assert_eq!(sm.state(), RecoveryState::Reconcile);
        sm.step().unwrap();
        assert_eq!(sm.state(), RecoveryState::Writable);
        // Terminal step is a no-op.
        sm.step().unwrap();
        assert_eq!(sm.state(), RecoveryState::Writable);
    }

    #[test]
    fn injected_faults_surface_and_rerun_converges() {
        // The §9.5 property: a crash at any boundary leaves the machine
        // in a state from which a re-run converges. Model a fault by
        // failing at a state; the re-run (fault removed) completes.
        for fault in ["probe", "replay", "checkpoint", "reconcile", "writable"] {
            let mut backend = FakeBackend::healthy();
            backend.fail_at = Some(fault);
            let mut sm = RecoveryStateMachine::new(&mut backend);
            let _ = sm.run_to_writable(); // Fault injects an error.
                                          // Crash happens here; the next mount re-runs from scratch.
            backend.fail_at = None;
            let mut sm2 = RecoveryStateMachine::new(&mut backend);
            sm2.run_to_writable()
                .expect("re-run must converge after the fault clears");
        }
    }

    #[test]
    fn audit_renders_human_readable() {
        let mut backend = FakeBackend::healthy();
        let mut sm = RecoveryStateMachine::new(&mut backend);
        sm.run_to_writable().unwrap();
        let line = sm.audit()[0].to_string();
        assert!(line.contains("generation 12"), "{line}");
        let line = sm.audit()[1].to_string();
        assert!(line.contains("3 rolled forward"), "{line}");
    }
}
