//! Write-ahead log (spec §6 step 9, §18) with:
//!
//! * **Commit markers** — a transaction's records are durable only when its
//!   `Commit` marker has been fsync'd; anything after the last complete
//!   committed transaction is discarded at replay (torn-write safe).
//! * **Per-record CRC32** — detects torn/partial writes at the tail.
//! * **Group commit** (spec §16.2) — a batch of records from concurrent
//!   writers shares one fsync; `stats()` reports the achieved amortization.
//! * **Refcount journaling** (audit C5 fix) — every refcount mutation is a
//!   logged, idempotent `RefOp` record, so dedup refcounts survive crashes.
//!
//! Record framing: `[u32 payload_len][u32 crc32(payload)][payload]`.
//!
//! Replay contract (spec §18 Mount()):
//! ```text
//! FOR entry IN wal_entries:
//!     IF entry.commit_marker present: replay
//!     ELSE:                          discard   # torn write, safe due to CoW
//! ```

use crate::core::Hash256;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Codec(#[from] bincode::Error),
    #[error("corrupt WAL tail at byte {0} (torn write)")]
    TornTail(u64),
}

/// One journaled mutation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WalRecord {
    /// Staged payload bytes (spec §6 step 9; `WalMode::Full` only). The
    /// payload is the *encrypted, compressed* form exactly as it will live
    /// in the data zone.
    Data { hash: Hash256, payload: Vec<u8> },
    /// A serialized `VersionNode` (namespace mutation).
    VNode { bytes: Vec<u8> },
    /// Directory child-map registration: `content` is the dir's
    /// `content_hash`; `children` its name → child-HEAD map.
    DirMap { content: Hash256, children: Vec<(String, Hash256)> },
    /// Refcount delta + location for a content hash — idempotent on replay
    /// (audit C5); the location and `stored` (hash of the on-device bytes)
    /// let MetadataOnly-mode replay rebuild the hash → extent map without
    /// re-reading payloads.
    RefOp { hash: Hash256, delta: i64, zone: u32, offset: u32, length: u32, stored: Hash256 },
    /// HEAD/root pointer swap (root hash + the root's timestamp).
    HeadOp { root: Hash256, ts: u64 },
    /// Snapshot bookkeeping.
    SnapshotOp { name: String, root: Hash256 },
    /// Transaction commit marker — the durability point.
    Commit { txn: u64 },
}

/// Group-commit telemetry (spec §16.2).
#[derive(Clone, Copy, Debug, Default)]
pub struct WalStats {
    pub transactions: u64,
    pub records: u64,
    pub fsyncs: u64,
    pub bytes_written: u64,
}

impl WalStats {
    /// Average records per fsync — the group-commit amortization factor.
    pub fn batch_size(&self) -> f64 {
        if self.fsyncs == 0 {
            0.0
        } else {
            self.records as f64 / self.fsyncs as f64
        }
    }
}

/// A file-backed WAL.
pub struct Wal {
    path: PathBuf,
    file: File,
    next_txn: u64,
    stats: WalStats,
}

impl Wal {
    /// Create (or truncate) a WAL at `path`.
    pub fn create(path: &Path) -> Result<Self, WalError> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(true)
            .open(path)?;
        Ok(Self { path: path.into(), file, next_txn: 1, stats: WalStats::default() })
    }

    /// Open an existing WAL for appending.
    pub fn open(path: &Path) -> Result<Self, WalError> {
        let mut file = OpenOptions::new().append(true).read(true).open(path)?;
        // Position at end for appends; validate we can parse the whole tail.
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len))?;
        let next_txn = Wal::replay(path)?.iter().map(|t| t.0).max().unwrap_or(0) + 1;
        Ok(Self { path: path.into(), file, next_txn, stats: WalStats::default() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stats(&self) -> WalStats {
        self.stats
    }

    /// Append a **batch** of records as one transaction (group commit):
    /// write all frames, then the commit marker, then a single fsync.
    pub fn commit_batch(&mut self, records: Vec<WalRecord>) -> Result<u64, WalError> {
        let txn = self.next_txn;
        self.next_txn += 1;

        let mut frames = records;
        frames.push(WalRecord::Commit { txn });
        let mut bytes_out = Vec::new();
        for record in &frames {
            let payload = bincode::serialize(record)?;
            let crc = crc32fast::hash(&payload);
            bytes_out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            bytes_out.extend_from_slice(&crc.to_be_bytes());
            bytes_out.extend_from_slice(&payload);
            self.stats.records += 1;
            if !matches!(record, WalRecord::Commit { .. }) {
                self.stats.bytes_written += payload.len() as u64;
            }
        }
        self.file.write_all(&bytes_out)?;
        self.file.sync_data()?; // THE durability point
        self.stats.transactions += 1;
        self.stats.fsyncs += 1;
        Ok(txn)
    }

    /// Replay all *committed* transactions from a WAL file.
    ///
    /// Returns `(txn_id, records)` pairs, in order. A trailing transaction
    /// without a commit marker is dropped (torn write), as is any trailing
    /// garbage after a CRC failure.
    pub fn replay(path: &Path) -> Result<Vec<(u64, Vec<WalRecord>)>, WalError> {
        let mut file = File::open(path)?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)?;

        let mut txns = Vec::new();
        let mut current: Vec<WalRecord> = Vec::new();
        let mut pos = 0usize;
        let len = raw.len();

        while pos + 8 <= len {
            let frame_len = u32::from_be_bytes(raw[pos..pos + 4].try_into().unwrap()) as usize;
            let frame_crc = u32::from_be_bytes(raw[pos + 4..pos + 8].try_into().unwrap());
            let body_start = pos + 8;
            if body_start + frame_len > len {
                // Truncated frame — torn write at the tail.
                break;
            }
            let body = &raw[body_start..body_start + frame_len];
            if crc32fast::hash(body) != frame_crc {
                // Corrupt frame — everything from here on is untrusted.
                break;
            }
            match bincode::deserialize::<WalRecord>(body) {
                Ok(WalRecord::Commit { txn }) => {
                    // Durability boundary: everything buffered so far is a
                    // committed transaction with this id.
                    txns.push((txn, std::mem::take(&mut current)));
                }
                Ok(record) => current.push(record),
                Err(_) => break, // undecodable — treat as torn tail
            }
            pos = body_start + frame_len;
        }

        // `current` non-empty here means an uncommitted tail → dropped.
        Ok(txns)
    }

    /// Reset the WAL after a checkpoint (spec §18: replay-since-checkpoint).
    pub fn reset(&mut self) -> Result<(), WalError> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.next_txn = 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample_records(n: usize) -> Vec<WalRecord> {
        (0..n)
            .map(|i| WalRecord::Data {
                hash: Hash256::of(&[i as u8]),
                payload: vec![i as u8; 64],
            })
            .collect()
    }

    #[test]
    fn commit_and_replay_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.commit_batch(sample_records(3)).unwrap();
            wal.commit_batch(vec![WalRecord::RefOp { hash: Hash256::of(b"x"), delta: 1, zone: 1, offset: 0, length: 64, stored: Hash256::of(b"stored") }]).unwrap();
        }
        let txns = Wal::replay(&path).unwrap();
        assert_eq!(txns.len(), 2);
        assert_eq!(txns[0].1.len(), 3);
        assert!(matches!(txns[0].1[0], WalRecord::Data { .. }));
        assert!(matches!(txns[1].1[0], WalRecord::RefOp { .. }));
    }

    #[test]
    fn torn_tail_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.commit_batch(sample_records(2)).unwrap();
            // Simulate a crash mid-transaction: append half a frame, no
            // commit marker, no fsync.
            let dangling = bincode::serialize(&WalRecord::Data {
                hash: Hash256::of(b"dangling"),
                payload: vec![9u8; 128],
            })
            .unwrap();
            let mut raw = Vec::new();
            raw.extend_from_slice(&((dangling.len() / 2) as u32).to_be_bytes());
            raw.extend_from_slice(&0u32.to_be_bytes());
            raw.extend_from_slice(&dangling[..dangling.len() / 2]);
            use std::io::Write;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&raw).unwrap();
        }
        let txns = Wal::replay(&path).unwrap();
        assert_eq!(txns.len(), 1, "torn tail must be dropped");
        assert_eq!(txns[0].1.len(), 2);
    }

    #[test]
    fn incomplete_txn_without_commit_marker_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.commit_batch(sample_records(1)).unwrap();
            // Write full frames of a second transaction but NEVER commit it.
            let record = WalRecord::Data { hash: Hash256::of(b"y"), payload: vec![7u8; 32] };
            let payload = bincode::serialize(&record).unwrap();
            let crc = crc32fast::hash(&payload);
            let mut raw = Vec::new();
            raw.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            raw.extend_from_slice(&crc.to_be_bytes());
            raw.extend_from_slice(&payload);
            use std::io::Write;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&raw).unwrap();
            f.sync_data().unwrap();
        }
        let txns = Wal::replay(&path).unwrap();
        assert_eq!(txns.len(), 1, "uncommitted transaction must be dropped");
    }

    #[test]
    fn reopen_appends_and_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.commit_batch(sample_records(1)).unwrap();
        }
        {
            let mut wal = Wal::open(&path).unwrap();
            wal.commit_batch(sample_records(2)).unwrap();
            let s = wal.stats();
            assert_eq!(s.transactions, 1);
            assert_eq!(s.fsyncs, 1);
            assert_eq!(s.batch_size(), 3.0); // 2 records + 1 commit marker
        }
        let txns = Wal::replay(&path).unwrap();
        assert_eq!(txns.len(), 2);
    }

    #[test]
    fn group_commit_amortizes_fsyncs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        let mut wal = Wal::create(&path).unwrap();
        // One writer, batch of 100 records → 1 fsync.
        wal.commit_batch(sample_records(100)).unwrap();
        let s = wal.stats();
        assert_eq!(s.fsyncs, 1);
        assert_eq!(s.records, 101); // + commit marker
        assert!(s.batch_size() > 100.0 / 1.0);
    }

    #[test]
    fn reset_after_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        let mut wal = Wal::create(&path).unwrap();
        wal.commit_batch(sample_records(5)).unwrap();
        wal.reset().unwrap();
        assert!(Wal::replay(&path).unwrap().is_empty());
        wal.commit_batch(sample_records(1)).unwrap();
        assert_eq!(Wal::replay(&path).unwrap().len(), 1);
    }
}
