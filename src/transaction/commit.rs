//! A pre-commit summary of a transaction, for logging/telemetry around
//! `TransactionManager::commit` without duplicating its actual disk I/O
//! logic here.

use crate::transaction::transaction::Transaction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitSummary {
    pub tx_id: u64,
    pub block_count: usize,
    pub total_bytes: usize,
}

pub fn summarize(tx: &Transaction) -> CommitSummary {
    CommitSummary {
        tx_id: tx.id,
        block_count: tx.dirty_blocks.len(),
        total_bytes: tx.dirty_blocks.values().map(|v| v.len()).sum(),
    }
}

/// Whether a transaction is worth committing at all -- an empty
/// transaction (nothing dirtied) doesn't need a journal write or a sync.
pub fn is_worth_committing(tx: &Transaction) -> bool {
    !tx.dirty_blocks.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_dirty_block_count_and_bytes() {
        let mut tx = Transaction::new(3, 0);
        tx.add_block(1, vec![0; 100]);
        tx.add_block(2, vec![0; 50]);
        let summary = summarize(&tx);
        assert_eq!(summary.tx_id, 3);
        assert_eq!(summary.block_count, 2);
        assert_eq!(summary.total_bytes, 150);
    }

    #[test]
    fn empty_transaction_is_not_worth_committing() {
        let tx = Transaction::new(1, 0);
        assert!(!is_worth_committing(&tx));
    }

    #[test]
    fn non_empty_transaction_is_worth_committing() {
        let mut tx = Transaction::new(1, 0);
        tx.add_block(5, vec![1]);
        assert!(is_worth_committing(&tx));
    }
}
