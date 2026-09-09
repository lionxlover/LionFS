use crate::disk::block_io::Disk;
use crate::ondisk::serialization::{
    JournalFooter, JournalHeader, JournalRecordHeader, Superblock, BLOCK_SIZE, JOURNAL_MAGIC,
};
use crate::transaction::transaction::Transaction;
use crate::utils::crc::compute_checksum;
use bytemuck::bytes_of;
use std::io::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

pub struct TransactionManager {
    pub current_tx_id: AtomicU64,
    pub next_journal_block: RwLock<u64>,
}

impl TransactionManager {
    pub fn new(sb: &Superblock) -> Self {
        Self {
            current_tx_id: AtomicU64::new(sb.generation + 1),
            next_journal_block: RwLock::new(0), // This will wrap around journal_blocks
        }
    }

    pub fn begin(&self, timestamp: u64) -> Transaction {
        let tx_id = self.current_tx_id.fetch_add(1, Ordering::SeqCst);
        Transaction::new(tx_id, timestamp)
    }

    pub fn commit(&self, disk: &Disk, sb: &Superblock, tx: &Transaction) -> Result<()> {
        if tx.dirty_blocks.is_empty() {
            return Ok(());
        }

        let entry_count = tx.dirty_blocks.len() as u32;

        let mut header = JournalHeader {
            magic: JOURNAL_MAGIC,
            version: 1,
            entry_count,
            tx_id: tx.id,
            timestamp: tx.timestamp,
            checksum: 0,
            padding_csum: 0,
            padding: [0; BLOCK_SIZE - 40],
        };

        header.checksum = compute_checksum(bytes_of(&header));

        // Sort keys to guarantee deterministic order and enable contiguous batching
        let mut sorted_keys: Vec<u64> = tx.dirty_blocks.keys().copied().collect();
        sorted_keys.sort_unstable();

        // Lock journal offset for sequential write
        let mut j_block_guard = self.next_journal_block.write().unwrap();
        let start_logical = *j_block_guard;
        let mut current_j_block = start_logical;

        let max_records = if sb.journal_blocks > 2 {
            ((sb.journal_blocks - 2) / 2) as usize
        } else {
            1
        };

        for chunk in sorted_keys.chunks(max_records) {
            let chunk_entry_count = chunk.len() as u32;
            let mut header = JournalHeader {
                magic: JOURNAL_MAGIC,
                version: 1,
                entry_count: chunk_entry_count,
                tx_id: tx.id,
                timestamp: tx.timestamp,
                checksum: 0,
                padding_csum: 0,
                padding: [0; BLOCK_SIZE - 40],
            };
            header.checksum = compute_checksum(bytes_of(&header));

            let total_journal_blocks = (2 * chunk_entry_count + 2) as u64;
            let mut journal_payload = Vec::with_capacity((total_journal_blocks as usize) * BLOCK_SIZE);
            journal_payload.extend_from_slice(bytes_of(&header));

            for &p_block in chunk {
                let data = &tx.dirty_blocks[&p_block];
                let data_checksum = compute_checksum(data);
                let rec_header = JournalRecordHeader {
                    tx_id: tx.id,
                    physical_block: p_block,
                    checksum: data_checksum,
                    padding: 0,
                    padding2: [0; BLOCK_SIZE - 24],
                };
                journal_payload.extend_from_slice(bytes_of(&rec_header));
                journal_payload.extend_from_slice(data);
            }

            let mut footer = JournalFooter {
                magic: JOURNAL_MAGIC,
                tx_id: tx.id,
                total_records: chunk_entry_count,
                checksum: 0,
                padding: [0; BLOCK_SIZE - 24],
            };
            footer.checksum = compute_checksum(bytes_of(&footer));
            journal_payload.extend_from_slice(bytes_of(&footer));

            if sb.journal_blocks > 0 {
                let start_idx = current_j_block % sb.journal_blocks;
                let blocks_to_end = sb.journal_blocks - start_idx;

                if total_journal_blocks <= blocks_to_end {
                    let p_start = sb.journal_start + start_idx;
                    disk.write_contiguous_run(p_start, &journal_payload)?;
                } else {
                    let split_byte = (blocks_to_end as usize) * BLOCK_SIZE;
                    let p_start = sb.journal_start + start_idx;
                    disk.write_contiguous_run(p_start, &journal_payload[..split_byte])?;
                    disk.write_contiguous_run(sb.journal_start, &journal_payload[split_byte..])?;
                }

                current_j_block += total_journal_blocks;
            }
        }

        // Linchpin WAL fsync: guarantees journal is durable before updating actual locations
        disk.sync()?;

        if sb.journal_blocks > 0 {
            *j_block_guard = current_j_block % sb.journal_blocks;
        }
        drop(j_block_guard); // Release lock early before applying to actual disk locations

        // Now apply to actual disk locations, coalescing contiguous block runs
        let mut i = 0;
        while i < sorted_keys.len() {
            let run_start = sorted_keys[i];
            let mut j = i + 1;
            while j < sorted_keys.len() && sorted_keys[j] == sorted_keys[j - 1] + 1 {
                j += 1;
            }
            let count = j - i;
            if count == 1 {
                disk.write_block(run_start, &tx.dirty_blocks[&run_start])?;
            } else {
                let mut run_buf = Vec::with_capacity(count * BLOCK_SIZE);
                for k in i..j {
                    run_buf.extend_from_slice(&tx.dirty_blocks[&sorted_keys[k]]);
                }
                disk.write_contiguous_run(run_start, &run_buf)?;
            }
            i = j;
        }

        // Flush final data
        disk.sync()?;

        Ok(())
    }
}
