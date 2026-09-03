//! Explicit transaction rollback/discard.
//!
//! Because `Transaction` only accumulates writes in an in-memory
//! `dirty_blocks` map until `TransactionManager::commit` actually writes
//! anything to disk, "rolling back" has always been implicitly possible by
//! just dropping the `Transaction` without committing it -- several
//! `fs::filesystem` handlers already do exactly that on an error path (the
//! `active_tx` simply isn't taken/committed). This module makes that
//! explicit and observable (what was discarded, and why) rather than a
//! silent drop, which matters for logging/debugging a failed operation.

use crate::transaction::transaction::Transaction;

#[derive(Debug, Clone)]
pub struct RollbackReport {
    pub tx_id: u64,
    pub discarded_block_count: usize,
    pub reason: String,
}

/// Consumes `tx`, discarding all of its buffered writes, and reports what
/// was thrown away. Nothing on disk is touched -- correct, because nothing
/// from this transaction was ever written to disk in the first place
/// (that only happens inside `TransactionManager::commit`).
pub fn rollback(tx: Transaction, reason: impl Into<String>) -> RollbackReport {
    RollbackReport {
        tx_id: tx.id,
        discarded_block_count: tx.dirty_blocks.len(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_how_many_blocks_were_discarded() {
        let mut tx = Transaction::new(7, 0);
        tx.add_block(1, vec![0; 10]);
        tx.add_block(2, vec![0; 10]);
        let report = rollback(tx, "write failed midway");
        assert_eq!(report.tx_id, 7);
        assert_eq!(report.discarded_block_count, 2);
        assert_eq!(report.reason, "write failed midway");
    }

    #[test]
    fn empty_transaction_rolls_back_cleanly() {
        let tx = Transaction::new(1, 0);
        let report = rollback(tx, "no-op");
        assert_eq!(report.discarded_block_count, 0);
    }
}
