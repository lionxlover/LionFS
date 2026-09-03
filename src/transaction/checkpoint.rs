//! Checkpointing: recording "every committed transaction up to and
//! including generation N has been fully applied to its final on-disk
//! locations," so crash recovery only needs to replay journal entries
//! *after* the last checkpoint rather than scanning the whole journal
//! region every time. `recovery::recovery::RecoveryManager::recover`
//! currently scans every journal slot looking for footers on every mount
//! regardless of how long ago they were applied; a checkpoint is what
//! would let it skip straight past old, already-applied ones. Not yet
//! wired into `TransactionManager`/`RecoveryManager` -- this is the
//! bookkeeping data structure that integration would use, kept separate
//! from the crash-consistency fix already made to `TransactionManager::commit`
//! in this pass, since two changes to the same crash-safety-critical code
//! path in one go is exactly the kind of compounding risk worth avoiding.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    pub last_applied_generation: u64,
    pub journal_position_at_checkpoint: u64,
}

impl Checkpoint {
    pub fn initial() -> Self {
        Self {
            last_applied_generation: 0,
            journal_position_at_checkpoint: 0,
        }
    }

    /// Whether a journal entry with the given transaction id has already
    /// been applied as of this checkpoint (and so can be skipped during
    /// replay).
    pub fn already_applied(&self, tx_id: u64) -> bool {
        tx_id <= self.last_applied_generation
    }

    pub fn advance(&mut self, applied_generation: u64, journal_position: u64) {
        if applied_generation > self.last_applied_generation {
            self.last_applied_generation = applied_generation;
            self.journal_position_at_checkpoint = journal_position;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_checkpoint_has_applied_nothing() {
        let cp = Checkpoint::initial();
        assert!(!cp.already_applied(1));
    }

    #[test]
    fn advancing_moves_the_watermark_forward_only() {
        let mut cp = Checkpoint::initial();
        cp.advance(10, 500);
        assert_eq!(cp.last_applied_generation, 10);
        cp.advance(5, 200); // stale/out-of-order update, should be ignored
        assert_eq!(cp.last_applied_generation, 10);
        cp.advance(20, 900);
        assert_eq!(cp.last_applied_generation, 20);
    }

    #[test]
    fn already_applied_check() {
        let mut cp = Checkpoint::initial();
        cp.advance(10, 0);
        assert!(cp.already_applied(5));
        assert!(cp.already_applied(10));
        assert!(!cp.already_applied(11));
    }
}
