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

        // Replay valid transactions in order
        let mut replayed = 0;
        for (tx_id, records) in tx_to_replay {
            if tx_id > sb.generation {
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
