use crate::disk::block_io::Disk;
use crate::ondisk::serialization::{
    JournalFooter, JournalHeader, JournalRecordHeader, Superblock, BLOCK_SIZE, JOURNAL_MAGIC,
};
use crate::utils::crc::compute_checksum;
use bytemuck::{bytes_of, pod_read_unaligned};
use std::collections::BTreeMap;
use std::io::Result;

pub struct RecoveryManager;

impl RecoveryManager {
    pub fn recover(disk: &mut Disk, sb: &Superblock) -> Result<u64> {
        if sb.journal_blocks == 0 {
            return Ok(0); // No journal to recover
        }

        let mut tx_to_replay = BTreeMap::new();
        let mut highest_tx = sb.generation;

        let mut current_block = 0;
        let mut buffer = vec![0u8; BLOCK_SIZE];

        while current_block < sb.journal_blocks {
            let p_block = sb.journal_start + current_block;
            disk.read_block(p_block, &mut buffer)?;

            let magic = u64::from_le_bytes(buffer[0..8].try_into().unwrap());
            if magic == JOURNAL_MAGIC {
                // Determine if it's a Header
                let mut header: JournalHeader = pod_read_unaligned(&buffer);
                let saved_checksum = header.checksum;
                header.checksum = 0;

                if compute_checksum(bytes_of(&header)) == saved_checksum {
                    // Valid header! Let's read its records
                    let mut records = Vec::new();
                    let mut valid = true;
                    let mut temp_block = current_block + 1;

                    for _ in 0..header.entry_count {
                        if temp_block >= sb.journal_blocks {
                            valid = false;
                            break;
                        }

                        let rec_p_block = sb.journal_start + temp_block;
                        disk.read_block(rec_p_block, &mut buffer)?;
                        let rec_header: JournalRecordHeader = pod_read_unaligned(&buffer);
                        temp_block += 1;

                        if temp_block >= sb.journal_blocks {
                            valid = false;
                            break;
                        }

                        let data_p_block = sb.journal_start + temp_block;
                        let mut data_buf = vec![0u8; BLOCK_SIZE];
                        disk.read_block(data_p_block, &mut data_buf)?;
                        temp_block += 1;

                        if compute_checksum(&data_buf) != rec_header.checksum {
                            valid = false;
                            break;
                        }

                        records.push((rec_header.physical_block, data_buf));
                    }

                    if valid && temp_block < sb.journal_blocks {
                        let footer_p_block = sb.journal_start + temp_block;
                        disk.read_block(footer_p_block, &mut buffer)?;
                        let mut footer: JournalFooter = pod_read_unaligned(&buffer);
                        let footer_checksum = footer.checksum;
                        footer.checksum = 0;

                        if footer.magic == JOURNAL_MAGIC
                            && footer.tx_id == header.tx_id
                            && compute_checksum(bytes_of(&footer)) == footer_checksum
                        {
                            // Fully valid transaction!
                            tx_to_replay.insert(header.tx_id, records);
                            if header.tx_id > highest_tx {
                                highest_tx = header.tx_id;
                            }
                            current_block = temp_block; // Skip past this transaction
                        }
                    }
                }
            }
            current_block += 1;
        }

        // Replay valid transactions in order -- but ONLY the
        // CONTIGUOUS-ID SUFFIX of the set above the superblock floor
        // (Phase 11 fix, found by the journal-wrapping money test).
        //
        // A wrapped journal holds a NON-contiguous set: the oldest
        // regions are overwritten by new transactions, so the scan can
        // find e.g. {2, 27..37, 65} with 3..26 and 38..64 gone.
        // Replaying the full set is UNSOUND: a transaction whose id
        // falls in a gap wrote newer versions of blocks the replayed
        // set never touches, so re-applying an old transaction's
        // blocks would push STALE tree-node contents over the live
        // (already-applied-in-place) tree -- a torn filesystem. The
        // WAL prefix property (the same discipline the deterministic
        // crash simulator asserts for the record log) says recovery
        // must apply exactly a contiguous suffix of the journal
        // sequence: the crash tail. Replay from the highest id
        // downward while ids are consecutive; stop at the first gap.
        // Each replayed transaction is then either the un-applied
        // crash tail (completing it is the point of recovery) or an
        // already-applied commit whose own newest content is
        // idempotent -- never a stale overwrite.
        let mut replay_ids: Vec<u64> = tx_to_replay
            .keys()
            .copied()
            .filter(|id| *id > sb.generation)
            .collect();
        if !replay_ids.is_empty() {
            // tx_to_replay is a BTreeMap: keys are ascending, so scan
            // from the END backwards, keeping the consecutive run.
            let mut cutoff = replay_ids.len() - 1;
            while cutoff > 0 && replay_ids[cutoff - 1] + 1 == replay_ids[cutoff] {
                cutoff -= 1;
            }
            replay_ids.drain(0..cutoff);
        }
        let mut replayed = 0;
        for tx_id in replay_ids {
            if let Some(records) = tx_to_replay.remove(&tx_id) {
                crate::debug::tracing::log_recovery_replay(tx_id, records.len());
                println!("Replaying transaction {}", tx_id);
                crate::recovery::replay::apply_records(disk, &records)?;
                replayed += 1;
            }
        }

        if replayed > 0 {
            disk.sync()?;
        }

        Ok(highest_tx)
    }
}

#[cfg(test)]
mod journal_wrap_tests {
    //! Phase 11 regression money tests: recovery must replay ONLY the
    //! contiguous-id suffix of the transactions found in a (possibly
    //! wrapped) journal. Found live by
    //! `fs::phase11_tests::pipelined_durable_two_writers_survive_remount`:
    //! 64 fsync groups wrap a 4096-block journal, the scan finds
    //! {old, gap, new} sets, and replaying the OLD transactions pushed
    //! stale tree-node contents over the live tree (torn filesystem:
    //! whole pages read back as zeros).

    use super::*;
    use crate::transaction::transaction::Transaction;

    fn test_sb() -> crate::ondisk::serialization::Superblock {
        // Small journal so a wrap is cheap to fabricate.
        crate::ondisk::serialization::Superblock {
            magic: crate::ondisk::serialization::LIONFS_MAGIC,
            block_size: 4096,
            journal_start: 4,
            journal_blocks: 64,
            generation: 1,
            ..unsafe { std::mem::zeroed() }
        }
    }

    /// Journal ONE transaction directly (the TransactionManager's
    /// on-disk format) without applying it.
    fn journal_only(
        disk: &mut Disk,
        sb: &crate::ondisk::serialization::Superblock,
        tx: &Transaction,
        j_offset: &mut u64,
    ) {
        let mut header = crate::ondisk::serialization::JournalHeader {
            magic: crate::ondisk::serialization::JOURNAL_MAGIC,
            version: 1,
            entry_count: tx.dirty_blocks.len() as u32,
            tx_id: tx.id,
            timestamp: tx.timestamp,
            checksum: 0,
            padding_csum: 0,
            padding: [0; BLOCK_SIZE - 40],
        };
        header.checksum = compute_checksum(bytes_of(&header));
        disk.write_block(
            sb.journal_start + (*j_offset % sb.journal_blocks),
            bytes_of(&header),
        )
        .unwrap();
        *j_offset += 1;
        let mut current = *j_offset;
        for (&p_block, data) in &tx.dirty_blocks {
            let rec_header = crate::ondisk::serialization::JournalRecordHeader {
                tx_id: tx.id,
                physical_block: p_block,
                checksum: compute_checksum(data),
                padding: 0,
                padding2: [0; BLOCK_SIZE - 24],
            };
            disk.write_block(
                sb.journal_start + (current % sb.journal_blocks),
                bytes_of(&rec_header),
            )
            .unwrap();
            current += 1;
            disk.write_block(sb.journal_start + (current % sb.journal_blocks), data)
                .unwrap();
            current += 1;
        }
        let mut footer = crate::ondisk::serialization::JournalFooter {
            magic: crate::ondisk::serialization::JOURNAL_MAGIC,
            tx_id: tx.id,
            total_records: tx.dirty_blocks.len() as u32,
            checksum: 0,
            padding: [0; BLOCK_SIZE - 24],
        };
        footer.checksum = compute_checksum(bytes_of(&footer));
        disk.write_block(
            sb.journal_start + (current % sb.journal_blocks),
            bytes_of(&footer),
        )
        .unwrap();
        // The next transaction starts AFTER the footer (current is the
        // footer position; the manager's next_journal_block ends past
        // it -- mirrored here).
        *j_offset = (current + 1) % sb.journal_blocks;
        disk.sync().unwrap();
    }

    /// THE money test: a wrapped journal containing an OLD transaction
    /// and a NEW transaction with an id GAP between them (the middle
    /// transactions were overwritten by the wrap). The old one wrote
    /// block 500 with content A; a MISSING (gap) transaction later
    /// rewrote block 500 with content B directly (applied, journal
    /// entry gone); the new transaction writes block 501. Recovery
    /// must replay ONLY the new transaction -- replaying the old one
    /// would push content A over the live content B (the stale
    /// overwrite that tore the filesystem in the wild).
    #[test]
    fn wrapped_journal_replays_only_the_contiguous_suffix() {
        let path = std::env::temp_dir().join(format!(
            "lionfs_wrap_test_{}.img",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut disk = Disk::create(&path, 1024 * 4096).unwrap();
        let sb = test_sb();

        let mut old_tx = Transaction::new(2, 0);
        old_tx.add_block(500, vec![0xAA; BLOCK_SIZE]);
        let mut j = 0u64;
        journal_only(&mut disk, &sb, &old_tx, &mut j);

        // The gap transaction (id 3): applied directly (journal entry
        // overwritten by the wrap); block 500 becomes "B" live.
        disk.write_block(500, &vec![0xBB; BLOCK_SIZE]).unwrap();

        // New transaction (id 4): writes block 501 = "C".
        let mut new_tx = Transaction::new(4, 0);
        new_tx.add_block(501, vec![0xCC; BLOCK_SIZE]);
        journal_only(&mut disk, &sb, &new_tx, &mut j);

        // Recovery sees {2, 4} with the gap at 3.
        let highest = RecoveryManager::recover(&mut disk, &sb).unwrap();
        assert_eq!(highest, 4, "the highest tx id is still reported");

        let mut got = vec![0u8; BLOCK_SIZE];
        disk.read_block(500, &mut got).unwrap();
        assert!(
            got.iter().all(|&b| b == 0xBB),
            "stale transaction must NOT be replayed across an id gap (block 500 corrupted)"
        );
        disk.read_block(501, &mut got).unwrap();
        assert!(
            got.iter().all(|&b| b == 0xCC),
            "the contiguous crash tail must still be replayed"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Contiguity across the floor: with {2, 3, 4} all present and
    /// sb.generation = 2, replay {3, 4} (the classic crash tail).
    #[test]
    fn contiguous_tail_above_the_floor_replays_in_order() {
        let path = std::env::temp_dir().join(format!(
            "lionfs_contig_test_{}.img",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut disk = Disk::create(&path, 1024 * 4096).unwrap();
        let sb = test_sb();

        let mut t2 = Transaction::new(2, 0);
        t2.add_block(600, vec![0x11; BLOCK_SIZE]);
        let mut j = 0u64;
        journal_only(&mut disk, &sb, &t2, &mut j);
        let mut t3 = Transaction::new(3, 0);
        t3.add_block(600, vec![0x22; BLOCK_SIZE]); // newer version
        journal_only(&mut disk, &sb, &t3, &mut j);
        let mut t4 = Transaction::new(4, 0);
        t4.add_block(601, vec![0x33; BLOCK_SIZE]);
        journal_only(&mut disk, &sb, &t4, &mut j);

        RecoveryManager::recover(&mut disk, &sb).unwrap();
        let mut got = vec![0u8; BLOCK_SIZE];
        disk.read_block(600, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0x22), "newest of the tail wins");
        disk.read_block(601, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0x33), "tail applied");
        let _ = std::fs::remove_file(&path);
    }
}
