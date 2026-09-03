//! Applying an already-validated set of journaled block writes to their
//! final on-disk locations. Extracted from
//! `recovery::recovery::RecoveryManager::recover`'s replay loop (which
//! calls this) so "how a validated transaction's records get applied" is
//! a small, independently testable/reusable unit, separate from the
//! (more involved, already-correct) logic that scans the journal region
//! and decides *which* transactions are valid and worth replaying.

use crate::disk::block_io::Disk;
use std::io::Result;

/// Writes every `(physical_block, data)` pair to disk, in order. Callers
/// are responsible for having already validated these records (checksum
/// verification, footer presence) -- this function trusts its input.
///
/// Uses `Disk::write_block_recovery` (Phase 3): on parity profiles the
/// apply always takes the full-row-recompute path, because replay of a
/// partially-applied transaction would feed the incremental RMW path an
/// old-data block that already equals the new data (delta 0) while
/// parity still belongs to the previous row -- the apply must be
/// IDEMPOTENT to be crash-safe, and only the recompute is.
pub fn apply_records(disk: &Disk, records: &[(u64, Vec<u8>)]) -> Result<usize> {
    for (physical_block, data) in records {
        disk.write_block_recovery(*physical_block, data)?;
    }
    Ok(records.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_every_record_and_reports_the_count() {
        let dir = std::env::temp_dir().join(format!("lionfs_replay_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.bin");
        let disk = Disk::create(&path, 64 * 4096).unwrap();

        let records = vec![(5u64, vec![0xAAu8; 4096]), (10u64, vec![0xBBu8; 4096])];
        let count = apply_records(&disk, &records).unwrap();
        assert_eq!(count, 2);

        let mut back = vec![0u8; 4096];
        disk.read_block(5, &mut back).unwrap();
        assert_eq!(back, vec![0xAAu8; 4096]);
        disk.read_block(10, &mut back).unwrap();
        assert_eq!(back, vec![0xBBu8; 4096]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
