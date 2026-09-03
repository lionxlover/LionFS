//! # Small-File Record Journal (RFC-004 §5)
//!
//! The measured problem this module exists for: a 4 KiB write to a new
//! file costs, on the metadata path, an inode insert + an extent insert
//! + a data block write -- three scattered device ops for one tiny
//! payload. Batching those three into *one* sequential log write is the
//! oldest trick in database engines (LMDB's freelist, RocksDB's WAL,
//! F2FS's log-structured origin story), and it is the reason this module
//! exists despite the B-epsilon tree already being write-optimized:
//! B-epsilon amortizes *random* index writes, not *small data* writes.
//!
//! The path (RFC-004 §5.1):
//!
//! ```text
//! small write (< SMALL_FILE_MAX) ──► RecordLog::append
//!                                     (header + payload, CRC32C'd)
//!                                     │
//!                        group-commit window (5 ms / 1 MiB, shared
//!                        with the io_engine batch)
//!                                     ▼
//!                        one sequential device write, N records
//!                                     │
//!                  reader path: overlay lookup hits the log first,
//!                  falls through to the B-epsilon tree on miss
//!                                     │
//!                  checkpoint (log > CHECKPOINT_BYTES or live ratio
//!                  < 50%): drain log into the B-epsilon tree, then
//!                  advance the superblock checkpoint watermark
//! ```
//!
//! Crash safety: the log is written-before-tree (the same discipline
//! as the metadata journal: RFC-002 §6), and each record is
//! self-describing (magic + length + CRC). Replay on mount reads the
//! log forward, stops at the first torn or corrupt record, and applies
//! every surviving record to the tree -- the tail of a partial batch
//! is simply discarded, exactly like a group-commit window that never
//! reached the device.
//!
//! This module is the *format and policy* layer; the tree-drain step
//! is wired by the transaction layer (see ROADMAP Phase 7). The types
//! here are deliberately sink-agnostic (`append` into any `Write`,
//! `replay` from any `Read`) so the deterministic simulator drives the
//! identical code the io_uring backend does.

use std::io::{self, Read, Write};

use crc32fast::Hasher as Crc32;

/// Record log magic: "LFSR" (LionFS Small-file Record log).
pub const LOG_MAGIC: [u8; 4] = *b"LFSR";
/// Record log format version.
pub const LOG_VERSION: u8 = 1;
/// Fixed header size in bytes.
pub const HEADER_BYTES: usize = 40;
/// Payloads at or below this size take the record-log path
/// (mirrors the v3 inode's inline threshold, RFC-002 §5.3).
pub const SMALL_FILE_MAX: u32 = 4032;

/// Record types. `Commit` closes a group-commit window (the durability
/// point); `Checkpoint` marks "everything before me is drained".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RecordType {
    /// Create + first write of a small file.
    Create = 1,
    /// Overwrite/append of data at a file offset.
    Data = 2,
    /// Delete (unlink).
    Delete = 3,
    /// Truncate to a new length.
    Truncate = 4,
    /// Durability marker: everything before this record is stable.
    Commit = 5,
    /// Drain marker: log before this sequence is in the tree.
    Checkpoint = 6,
}

impl RecordType {
    #[must_use]
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Create),
            2 => Some(Self::Data),
            3 => Some(Self::Delete),
            4 => Some(Self::Truncate),
            5 => Some(Self::Commit),
            6 => Some(Self::Checkpoint),
            _ => None,
        }
    }
}

/// A validated record as produced by replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub kind: RecordType,
    pub file_id: u64,
    /// Byte offset within the file for `Data`, new length for
    /// `Truncate`, zero otherwise.
    pub offset: u64,
    pub sequence: u64,
    pub payload: Vec<u8>,
}

/// Append-side: builds the on-wire record.
fn encode_record(
    kind: RecordType,
    file_id: u64,
    offset: u64,
    sequence: u64,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_BYTES + payload.len());
    // Header: magic(4) | version(1) | type(1) | flags(2) | reserved(4)
    //        | file_id(8) | offset(8) | sequence(8) | payload_len(4)
    //        | header_crc... no: single crc over header+payload, last.
    // Total header = 40 bytes, then payload, then CRC32 (4 bytes) over
    // everything before it. Torn writes are detected by the CRC.
    buf.extend_from_slice(&LOG_MAGIC);
    buf.push(LOG_VERSION);
    buf.push(kind as u8);
    buf.extend_from_slice(&[0u8; 2]); // flags
    buf.extend_from_slice(&[0u8; 4]); // reserved
    buf.extend_from_slice(&file_id.to_le_bytes());
    buf.extend_from_slice(&offset.to_le_bytes());
    buf.extend_from_slice(&sequence.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    debug_assert_eq!(buf.len(), 40);
    buf.extend_from_slice(payload);
    let mut crc = Crc32::new();
    crc.update(&buf);
    buf.extend_from_slice(&crc.finalize().to_le_bytes());
    buf
}

/// The replay-side header, decoded but not yet payload-validated.
struct DecodedHeader {
    kind: RecordType,
    file_id: u64,
    offset: u64,
    sequence: u64,
    payload_len: u32,
    total: usize,
}

/// Tries to decode one record header from `src`. Returns:
/// * `Ok(Some(header))` -- header valid, `total` bytes to consume
/// * `Ok(None)` -- clean end of log (0 trailing bytes)
/// * `Err(TailState::Torn)` -- trailing bytes that are not a full
///   header: a torn write; replay stops, discarding the tail.
/// * `Err(TailState::Corrupt)` -- full header but bad magic/version:
///   corruption, not tearing; replay stops and reports.
fn decode_header(src: &[u8]) -> Result<Option<DecodedHeader>, TailState> {
    if src.is_empty() {
        return Ok(None);
    }
    if src.len() < HEADER_BYTES {
        return Err(TailState::Torn);
    }
    if src[0..4] != LOG_MAGIC || src[4] != LOG_VERSION {
        return Err(TailState::Corrupt);
    }
    let kind = RecordType::from_tag(src[5]).ok_or(TailState::Corrupt)?;
    let file_id = u64::from_le_bytes(src[12..20].try_into().expect("8 bytes"));
    let offset = u64::from_le_bytes(src[20..28].try_into().expect("8 bytes"));
    let sequence = u64::from_le_bytes(src[28..36].try_into().expect("8 bytes"));
    let payload_len = u32::from_le_bytes(src[36..40].try_into().expect("4 bytes"));
    let total = HEADER_BYTES + payload_len as usize + 4;
    if src.len() < total {
        return Err(TailState::Torn);
    }
    Ok(Some(DecodedHeader {
        kind,
        file_id,
        offset,
        sequence,
        payload_len,
        total,
    }))
}

/// Why replay stopped early.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TailState {
    /// The log ended mid-record: a partial window write. The tail is
    /// discarded; this is *normal* after a crash mid-batch.
    Torn,
    /// A complete header failed validation (magic/version/type): real
    /// corruption. Surfaced to the healer (RFC-002 §7) for the
    /// quarantine path.
    Corrupt,
}

/// Replay statistics, for mount-time reporting (RFC-004 §8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub applied: u64,
    pub bytes: u64,
    /// Highest sequence applied (the new checkpoint watermark
    /// candidate).
    pub last_sequence: u64,
    /// How the replay ended (None = clean end of log).
    pub tail: Option<TailState>,
}

/// The append-side log. `W` is the device sink (in tests, a `Vec<u8>`;
/// in the engine, the group-commit batch buffer).
pub struct RecordLog<W: Write> {
    sink: W,
    sequence: u64,
    /// Bytes appended since the last checkpoint.
    bytes_since_checkpoint: u64,
    /// Records appended since the last checkpoint.
    records_since_checkpoint: u64,
    last_kind: Option<RecordType>,
}

impl<W: Write> RecordLog<W> {
    /// A fresh log over `sink` with sequence numbering from zero.
    pub fn new(sink: W) -> Self {
        Self {
            sink,
            sequence: 0,
            bytes_since_checkpoint: 0,
            records_since_checkpoint: 0,
            last_kind: None,
        }
    }

    /// Access to the sink (ownership transfer on close).
    pub fn into_inner(self) -> W {
        self.sink
    }

    /// Current sequence watermark.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Appends one record. Small-file enforcement (`Data`/`Create`
    /// payloads must be ≤ [`SMALL_FILE_MAX`]) is a hard invariant of
    /// the path: a bigger payload on this path is a caller bug, and
    /// it is refused before any bytes touch the sink.
    pub fn append(&mut self, kind: RecordType, file_id: u64, offset: u64, payload: &[u8]) -> io::Result<u64> {
        match kind {
            RecordType::Create | RecordType::Data => {
                if payload.len() > SMALL_FILE_MAX as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "payload exceeds the small-file record path threshold",
                    ));
                }
            }
            RecordType::Delete | RecordType::Truncate => {
                if !payload.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "control records carry no payload",
                    ));
                }
            }
            RecordType::Commit | RecordType::Checkpoint => {}
        }
        let seq = self.sequence;
        let wire = encode_record(kind, file_id, offset, seq, payload);
        self.sink.write_all(&wire)?;
        self.sequence += 1;
        self.bytes_since_checkpoint += wire.len() as u64;
        self.records_since_checkpoint += 1;
        self.last_kind = Some(kind);
        Ok(seq)
    }

    /// Appends a `Commit` record and flushes the sink: the durability
    /// point of a group-commit window. Everything appended before this
    /// call is stable once it returns.
    pub fn commit(&mut self) -> io::Result<u64> {
        let seq = self.append(RecordType::Commit, 0, 0, &[])?;
        self.sink.flush()?;
        Ok(seq)
    }

    /// Writes a `Checkpoint` marker (sequence watermark: everything
    /// ≤ `drained_through` now lives in the B-epsilon tree). Does not
    /// truncate the log; physical truncation is the drain step's job,
    /// after the superblock records the watermark.
    pub fn mark_checkpoint(&mut self, drained_through: u64) -> io::Result<u64> {
        let seq = self.append(RecordType::Checkpoint, 0, drained_through, &[])?;
        self.bytes_since_checkpoint = 0;
        self.records_since_checkpoint = 0;
        self.sink.flush()?;
        Ok(seq)
    }

    /// Whether a checkpoint is due: byte budget exceeded, record budget
    /// exceeded, or the log ends in a control burst (many `Commit`s
    /// with no data between them -- chatty small-file workloads).
    #[must_use]
    pub fn checkpoint_due(&self, byte_budget: u64, record_budget: u64) -> bool {
        self.bytes_since_checkpoint >= byte_budget
            || self.records_since_checkpoint >= record_budget
            || (self.last_kind == Some(RecordType::Commit)
                && self.records_since_checkpoint > 1
                && self.bytes_since_checkpoint > 0)
    }

    #[must_use]
    pub fn bytes_since_checkpoint(&self) -> u64 {
        self.bytes_since_checkpoint
    }

    #[must_use]
    pub fn records_since_checkpoint(&self) -> u64 {
        self.records_since_checkpoint
    }
}

/// Replays a log image (`&[u8]` -- read whole from the device region in
/// production, or a `Vec` in tests). Returns the surviving entries in
/// sequence order plus how the replay ended. A torn tail is normal and
/// silently discarded; a corrupt header is reported in `stats.tail`.
pub fn replay(image: &[u8]) -> (Vec<LogEntry>, ReplayStats) {
    let mut out = Vec::new();
    let mut stats = ReplayStats::default();
    let mut rest = image;
    loop {
        match decode_header(rest) {
            Ok(None) => break,
            Err(state) => {
                stats.tail = Some(state);
                break;
            }
            Ok(Some(hdr)) => {
                let body = &rest[HEADER_BYTES..HEADER_BYTES + hdr.payload_len as usize];
                let crc_wire = u32::from_le_bytes(
                    rest[hdr.total - 4..hdr.total]
                        .try_into()
                        .expect("4 bytes"),
                );
                let mut crc = Crc32::new();
                crc.update(&rest[..hdr.total - 4]);
                if crc.finalize() != crc_wire {
                    // A complete-length record failing CRC is a torn
                    // (or corrupted) write; either way the tail stops
                    // here and nothing after it is trusted.
                    stats.tail = Some(TailState::Torn);
                    break;
                }
                out.push(LogEntry {
                    kind: hdr.kind,
                    file_id: hdr.file_id,
                    offset: hdr.offset,
                    sequence: hdr.sequence,
                    payload: body.to_vec(),
                });
                stats.applied += 1;
                stats.bytes += hdr.total as u64;
                stats.last_sequence = hdr.sequence;
                rest = &rest[hdr.total..];
            }
        }
    }
    (out, stats)
}

/// Replays from any `Read` (streamed replay for large logs). The
/// caller supplies the full image via `read_to_end`; this is a
/// convenience wrapper for tooling (`lfs_dump --recordlog`).
pub fn replay_stream<R: Read>(mut src: R) -> io::Result<(Vec<LogEntry>, ReplayStats)> {
    let mut image = Vec::new();
    src.read_to_end(&mut image)?;
    Ok(replay(&image))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_replay_roundtrip() {
        let mut log = RecordLog::new(Vec::new());
        log.append(RecordType::Create, 7, 0, b"hello").expect("write");
        log.append(RecordType::Data, 7, 5, b" world").expect("write");
        log.append(RecordType::Delete, 8, 0, &[]).expect("write");
        let seq = log.commit().expect("commit");
        let image = log.into_inner();

        let (entries, stats) = replay(&image);
        assert_eq!(stats.applied, 4);
        assert_eq!(stats.tail, None);
        assert_eq!(stats.last_sequence, seq);
        assert_eq!(entries[0].kind, RecordType::Create);
        assert_eq!(entries[0].payload, b"hello");
        assert_eq!(entries[1].payload, b" world");
        assert_eq!(entries[1].offset, 5);
        assert_eq!(entries[2].kind, RecordType::Delete);
        // Sequences are dense and ordered.
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.sequence, i as u64);
        }
    }

    #[test]
    fn torn_tail_is_discarded_silently() {
        let mut log = RecordLog::new(Vec::new());
        log.append(RecordType::Create, 7, 0, b"hello").expect("write");
        log.commit().expect("commit");
        let mut image = log.into_inner();
        let full = image.clone();
        image.extend_from_slice(&full[full.len() - 3..]); // garbage tail

        let (entries, stats) = replay(&image);
        assert_eq!(entries.len(), 2);
        assert_eq!(stats.tail, Some(TailState::Torn));
        assert_eq!(stats.applied, 2);
    }

    #[test]
    fn truncated_mid_record_is_torn() {
        let mut log = RecordLog::new(Vec::new());
        log.append(RecordType::Create, 7, 0, &[0xAB; 512]).expect("write");
        let image = log.into_inner();
        // Cut inside the payload.
        let (entries, stats) = replay(&image[..HEADER_BYTES + 100]);
        assert!(entries.is_empty());
        assert_eq!(stats.tail, Some(TailState::Torn));
        // Cut inside the header itself.
        let (entries, stats) = replay(&image[..10]);
        assert!(entries.is_empty());
        assert_eq!(stats.tail, Some(TailState::Torn));
    }

    #[test]
    fn crc_failure_stops_replay() {
        let mut log = RecordLog::new(Vec::new());
        log.append(RecordType::Create, 7, 0, b"hello").expect("write");
        log.commit().expect("commit");
        log.append(RecordType::Data, 7, 5, b" world").expect("write");
        let mut image = log.into_inner();
        // Corrupt the second record's payload (after first commit).
        let commit_end = image.len() - HEADER_BYTES - 6 - 4;
        image[commit_end + 10] ^= 0xFF;

        let (entries, stats) = replay(&image);
        // First two records (Create + Commit) survive; the corrupt one
        // and nothing after it applies.
        assert_eq!(entries.len(), 2);
        assert_eq!(stats.tail, Some(TailState::Torn));
    }

    #[test]
    fn bad_magic_is_corruption_not_tearing() {
        let mut image = Vec::new();
        image.extend_from_slice(b"XXXX");
        image.extend_from_slice(&[0u8; HEADER_BYTES - 4]);
        let (_, stats) = replay(&image);
        assert_eq!(stats.tail, Some(TailState::Corrupt));
    }

    #[test]
    fn empty_log_replays_cleanly() {
        let (entries, stats) = replay(&[]);
        assert!(entries.is_empty());
        assert_eq!(stats, ReplayStats::default());
    }

    #[test]
    fn small_file_enforcement() {
        let mut log = RecordLog::new(Vec::new());
        // Exactly at the threshold is fine.
        log.append(RecordType::Data, 1, 0, &[0u8; SMALL_FILE_MAX as usize])
            .expect("at threshold");
        // One byte over is refused before touching the sink.
        let err = log
            .append(RecordType::Data, 1, 0, &[0u8; SMALL_FILE_MAX as usize + 1])
            .expect_err("over threshold");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // Control records with payloads are refused.
        let err = log
            .append(RecordType::Delete, 1, 0, b"x")
            .expect_err("no payload on Delete");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // Nothing was appended by the failed calls.
        assert_eq!(log.sequence(), 1);
    }

    #[test]
    fn checkpoint_due_policy() {
        // Record budget: 8 records appended.
        let mut log = RecordLog::new(Vec::new());
        for i in 0..8 {
            log.append(RecordType::Data, 1, i * 100, &[7u8; 100]).expect("write");
        }
        assert!(log.checkpoint_due(1 << 20, 8));
        assert!(!log.checkpoint_due(1 << 20, 9));
        // Byte budget: one 4032-byte payload totals 4076 wire bytes.
        let mut log2 = RecordLog::new(Vec::new());
        log2.append(RecordType::Data, 1, 0, &[0u8; 4032]).expect("write");
        assert!(!log2.checkpoint_due(4096, 100)); // 4076 < 4096
        assert!(log2.checkpoint_due(4000, 100)); // 4076 >= 4000
    }

    #[test]
    fn checkpoint_marker_resets_counters() {
        let mut log = RecordLog::new(Vec::new());
        for i in 0..4 {
            log.append(RecordType::Data, 1, i * 10, &[9u8; 10]).expect("write");
        }
        assert!(log.bytes_since_checkpoint() > 0);
        log.mark_checkpoint(4).expect("mark");
        assert_eq!(log.bytes_since_checkpoint(), 0);
        assert_eq!(log.records_since_checkpoint(), 0);
        assert!(!log.checkpoint_due(1, 1));
    }

    #[test]
    fn checkpoint_marker_survives_replay() {
        let mut log = RecordLog::new(Vec::new());
        log.append(RecordType::Data, 1, 0, b"abc").expect("write");
        log.commit().expect("commit");
        log.mark_checkpoint(2).expect("mark");
        let image = log.into_inner();
        let (entries, stats) = replay(&image);
        assert_eq!(entries.len(), 3);
        let cp = &entries[2];
        assert_eq!(cp.kind, RecordType::Checkpoint);
        assert_eq!(cp.offset, 2); // drained-through watermark field
        assert_eq!(stats.last_sequence, 2);
    }

    #[test]
    fn stream_replay_matches_buffer_replay() {
        let mut log = RecordLog::new(Vec::new());
        log.append(RecordType::Create, 3, 0, b"one").expect("write");
        log.append(RecordType::Data, 3, 3, b"two").expect("write");
        log.commit().expect("commit");
        let image = log.into_inner();
        let (a, sa) = replay(&image);
        let (b, sb) = replay_stream(std::io::Cursor::new(image)).expect("stream");
        assert_eq!(a, b);
        assert_eq!(sa, sb);
    }

    #[test]
    fn records_are_40_byte_headers_plus_payload_plus_crc() {
        let wire = encode_record(RecordType::Data, 1, 2, 3, b"payload");
        assert_eq!(wire.len(), HEADER_BYTES + 7 + 4);
    }
}
