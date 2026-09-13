//! Zoned, log-structured extent store (spec §4) with a crash-accurate
//! device model.
//!
//! Layout on one device (prototype: a single "disk"; production spreads
//! zones across devices with EC):
//!
//! ```text
//! ┌──────────────┐  3 × 4 KiB superblock copies (epoch + CRC, majority vote)
//! │ SUPERBLOCK×3 │
//! ├──────────────┤  zone map (bitmap + per-zone health)
//! │ ZONE MAP     │
//! ├──────────────┤  journal zone (owned by hfs-wal)
//! │ JOURNAL      │
//! ├──────────────┤  data zones: append-only, segment-granular cleaning
//! │ DATA ZONES   │  extent = (zone, offset, length)
//! └──────────────┘
//! ```
//!
//! **Crash semantics**: [`FileDevice`] keeps writes in a volatile overlay;
//! nothing is durable until [`BlockDevice::sync`]. Dropping the device
//! without syncing is the simulated power cut — integration tests recover
//! from exactly that state via the WAL.
//!
//! **Superblock** (spec §4): triple-mirrored, versioned by epoch, CRC'd;
//! [`Store::open`] takes the valid copy with the highest epoch (majority
//! not required for correctness here because each copy is self-verifying —
//! the audit's G4 fix: the HEAD root pointer is *inside* the epoch'd,
//! CRC'd superblock, so a torn 32-byte pointer write can never be observed).

use crate::core::{CompressionType, ExtentDescriptor, Hash256, extent_flags};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("serialization: {0}")]
    Codec(#[from] bincode::Error),
    #[error("no valid superblock copy found (all 3 corrupt?)")]
    NoValidSuperblock,
    #[error("zone {0} out of range")]
    ZoneOutOfRange(u32),
    #[error("extent corrupt: stored hash {stored} != computed {computed}")]
    ChecksumMismatch { stored: Hash256, computed: Hash256 },
    #[error("store full: no zone has room for {0} bytes")]
    Full(usize),
    #[error("decompression failed: {0}")]
    Decompress(String),
    #[error("corrupt store state: {0}")]
    Corrupt(String),
}

// ---------------------------------------------------------------------------
// Device model
// ---------------------------------------------------------------------------

/// A block device with explicit durability points.
pub trait BlockDevice {
    /// Volatile write (durable only after `sync`).
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>>;
    /// Durability point — flush volatile writes to stable storage.
    fn sync(&mut self) -> io::Result<()>;
    fn len(&self) -> u64;
    /// Whether the device has zero capacity.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-memory device (tests / volatile tiers).
pub struct InMemDevice {
    data: Vec<u8>,
}

impl InMemDevice {
    pub fn new(len: u64) -> Self {
        Self { data: vec![0u8; len as usize] }
    }
}

impl BlockDevice for InMemDevice {
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let end = (offset + data.len() as u64) as usize;
        if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[offset as usize..end].copy_from_slice(data);
        Ok(())
    }

    fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let end = (offset + len) as usize;
        if end > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past device end"));
        }
        Ok(self.data[offset as usize..end].to_vec())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(()) // memory is already "durable" for test purposes
    }

    fn len(&self) -> u64 {
        self.data.len() as u64
    }
}

/// File-backed device with a volatile write-back overlay: reads see the
/// overlay, durability happens only at `sync()`. Simulates OS page cache +
/// power cut for the crash-recovery tests.
pub struct FileDevice {
    file: File,
    durable_len: u64,
    overlay: Vec<(u64, Vec<u8>)>,
    capacity: u64,
}

impl FileDevice {
    pub fn create(path: &Path, capacity: u64) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(capacity)?;
        Ok(Self { file, durable_len: 0, overlay: Vec::new(), capacity })
    }

    pub fn open(path: &Path, capacity: u64) -> io::Result<Self> {
        let file = File::options().read(true).write(true).open(path)?;
        let durable_len = file.metadata()?.len().min(capacity);
        Ok(Self { file, durable_len, overlay: Vec::new(), capacity })
    }
}

impl BlockDevice for FileDevice {
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        if offset + data.len() as u64 > self.capacity {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write past device capacity"));
        }
        // Coalesce: replace any fully-covered overlay entry.
        self.overlay.retain(|(off, buf)| {
            !(offset <= *off && *off + buf.len() as u64 <= offset + data.len() as u64)
        });
        self.overlay.push((offset, data.to_vec()));
        Ok(())
    }

    fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let end = offset + len;
        if end > self.capacity {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past device end"));
        }
        // Positioned read (no cursor, &self) via FileExt.
        use std::os::unix::fs::FileExt;
        let mut out = vec![0u8; len as usize];
        self.file.read_exact_at(&mut out, offset)?;
        // Apply overlay.
        for (off, buf) in &self.overlay {
            let s = (*off).max(offset);
            let e = (*off + buf.len() as u64).min(end);
            if s < e {
                let buf_start = (s - off) as usize;
                let out_start = (s - offset) as usize;
                let n = (e - s) as usize;
                out[out_start..out_start + n].copy_from_slice(&buf[buf_start..buf_start + n]);
            }
        }
        Ok(out)
    }

    fn sync(&mut self) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        // Sort overlay by offset for sequential-ish writes.
        self.overlay.sort_by_key(|(off, _)| *off);
        for (off, buf) in std::mem::take(&mut self.overlay) {
            self.file.write_all_at(buf.as_slice(), off)?;
            self.durable_len = self.durable_len.max(off + buf.len() as u64);
        }
        self.file.sync_all()?;
        Ok(())
    }

    fn len(&self) -> u64 {
        self.durable_len.max(self.overlay.iter().map(|(o, b)| o + b.len() as u64).max().unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------
// Superblock
// ---------------------------------------------------------------------------

const SB_MAGIC: u32 = 0x48465331; // "LionFS"
const SB_COPY_SIZE: u64 = 4096;
const SB_COPIES: u64 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Superblock {
    pub magic: u32,
    pub version: u32,
    /// Monotonic superblock generation (torn-write guard, audit G4).
    pub epoch: u64,
    /// Current root DAG hash — the filesystem HEAD (double-protected: epoch
    /// + CRC mean a torn pointer write is undetectable-but-unobservable).
    pub root: Hash256,
    /// Reference to the engine checkpoint blob living in the dedicated
    /// checkpoint area (offset, length, CRC of the blob).
    pub checkpoint: CheckpointRef,
}

/// Pointer to the checkpoint blob in the checkpoint area.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default)]
pub struct CheckpointRef {
    pub offset: u64,
    pub len: u32,
    pub crc: u32,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Geometry of the store.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct StoreGeometry {
    pub zone_count: u32,
    pub zone_size: u64,
    pub segment_size: u32,
}

impl StoreGeometry {
    pub fn data_start(&self) -> u64 {
        (SB_COPY_SIZE * SB_COPIES) + self.zone_map_size() + self.checkpoint_area_size()
    }

    pub fn zone_map_size(&self) -> u64 {
        4096
    }

    /// Dedicated checkpoint area (single-slot, CRC-protected).
    pub fn checkpoint_area_size(&self) -> u64 {
        8 * 1024 * 1024
    }

    pub fn checkpoint_offset(&self) -> u64 {
        (SB_COPY_SIZE * SB_COPIES) + self.zone_map_size()
    }

    pub fn zone_offset(&self, zone_id: u32) -> Result<u64, StoreError> {
        if zone_id >= self.zone_count {
            return Err(StoreError::ZoneOutOfRange(zone_id));
        }
        Ok(self.data_start() + zone_id as u64 * self.zone_size)
    }

    pub fn total_size(&self) -> u64 {
        self.data_start() + self.zone_count as u64 * self.zone_size
    }
}

/// One segment's bookkeeping (for the cleaner, spec §8).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub zone_id: u32,
    pub segment_index: u32,
    /// Extents written into this segment: (descriptor, content hash).
    pub extents: Vec<(ExtentDescriptor, Hash256)>,
    /// Seconds since epoch when the segment was closed (0 = still open).
    pub closed_at_secs: u64,
}

impl SegmentMeta {
    pub fn seg_start_offset(&self, geo: &StoreGeometry) -> u32 {
        self.segment_index * geo.segment_size
    }
}

/// Cleaner-facing view of one closed segment (spec §8.1 cost-benefit).
#[derive(Clone, Copy, Debug)]
pub struct SegmentInfo {
    /// Index into `closed_segments`.
    pub seg_idx: usize,
    pub zone_id: u32,
    pub segment_index: u32,
    /// Bytes of live extents over total bytes written.
    pub live_fraction: f64,
    pub total_bytes: u64,
    pub live_bytes: u64,
    /// Whether any extent carries an EC group id (defense-in-depth: the
    /// cleaner refuses to relocate such extents — see
    /// [`Store::clean_segment`]).
    pub has_ec_extents: bool,
}

/// Report of one cleaner pass over a segment (spec §8.2).
#[derive(Clone, Copy, Debug, Default)]
pub struct CleanReport {
    /// Live extents rewritten elsewhere.
    pub extents_moved: usize,
    /// Bytes of live data rewritten.
    pub bytes_moved: u64,
    /// Segment bytes returned to the free lists.
    pub bytes_reclaimed: u64,
}

/// Persistent allocator / reference state — everything `Store::open` cannot
/// reconstruct from the device (v1.1: cursors used to reset to zero, which
/// silently clobbered live extents after remount; refcounts vanished, which
/// would have made the cleaner free live data). The engine serializes this
/// into its checkpoint and hands it back via [`Store::restore`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreState {
    pub zone_cursors: Vec<u64>,
    pub closed_segments: Vec<SegmentMeta>,
    /// (hash, count) pairs — HashMaps serialize poorly with bincode.
    pub refcounts: Vec<(Hash256, u64)>,
    /// Per-zone freed byte ranges available for reuse: (offset, len).
    pub free_lists: Vec<Vec<(u32, u32)>>,
}

/// The zoned extent store.
pub struct Store {
    device: Box<dyn BlockDevice>,
    geo: StoreGeometry,
    /// Open (append-in-progress) segment per zone.
    open_segments: Vec<Option<SegmentMeta>>,
    /// Closed segments awaiting cleaning (global list).
    pub closed_segments: Vec<SegmentMeta>,
    /// Allocation cursor per zone (offset within the zone).
    zone_cursors: Vec<u64>,
    /// Content-hash refcounts (spec §8.2: live = refcount > 0).
    pub refcounts: std::collections::HashMap<Hash256, u64>,
    /// Reclaimed byte ranges per zone, consumed before the cursor (GC).
    free_lists: Vec<Vec<(u32, u32)>>,
    epoch: u64,
    root: Hash256,
    checkpoint: CheckpointRef,
}

impl Store {
    /// Format a fresh store on `device`.
    pub fn format(
        mut device: Box<dyn BlockDevice>,
        zone_count: u32,
        zone_size: u64,
        segment_size: u32,
    ) -> Result<Self, StoreError> {
        let geo = StoreGeometry { zone_count, zone_size, segment_size };
        let total = geo.total_size();
        if device.len() < total {
            // Grow the device to geometry.
            device.write(total - 1, &[0])?;
        }
        let mut store = Self {
            device,
            geo,
            open_segments: (0..zone_count).map(|_| None).collect(),
            closed_segments: Vec::new(),
            zone_cursors: vec![0; zone_count as usize],
            refcounts: std::collections::HashMap::new(),
            free_lists: (0..zone_count).map(|_| Vec::new()).collect(),
            epoch: 0,
            root: Hash256::zero(),
            checkpoint: CheckpointRef::default(),
        };
        store.write_geometry()?;
        store.write_superblock(Hash256::zero(), CheckpointRef::default())?;
        Ok(store)
    }

    /// Open an existing store: majority-of-three superblock read.
    pub fn open(device: Box<dyn BlockDevice>) -> Result<Self, StoreError> {
        let candidates: Vec<Superblock> = (0..SB_COPIES)
            .filter_map(|i| Self::read_sb_copy(device.as_ref(), i))
            .collect();
        let sb = candidates
            .into_iter()
            .max_by_key(|sb| sb.epoch)
            .ok_or(StoreError::NoValidSuperblock)?;

        // Geometry lives in the zone-map header in the prototype (the
        // first bytes of the area after the superblocks). Read it.
        let geo = Self::read_geometry(device.as_ref())?;
        Ok(Self {
            device,
            geo,
            open_segments: (0..geo.zone_count).map(|_| None).collect(),
            closed_segments: Vec::new(),
            zone_cursors: vec![0; geo.zone_count as usize],
            refcounts: std::collections::HashMap::new(),
            free_lists: (0..geo.zone_count).map(|_| Vec::new()).collect(),
            epoch: sb.epoch,
            root: sb.root,
            checkpoint: sb.checkpoint,
        })
    }

    fn read_sb_copy(device: &dyn BlockDevice, index: u64) -> Option<Superblock> {
        let raw = device.read(index * SB_COPY_SIZE, SB_COPY_SIZE).ok()?;
        if raw.len() < 16 {
            return None;
        }
        let len = u32::from_be_bytes(raw[0..4].try_into().ok()?) as usize;
        let crc = u32::from_be_bytes(raw[4..8].try_into().ok()?);
        if 16 + len > raw.len() {
            return None;
        }
        let body = &raw[16..16 + len];
        if crc32fast::hash(body) != crc {
            return None;
        }
        let sb: Superblock = bincode::deserialize(body).ok()?;
        if sb.magic != SB_MAGIC {
            return None;
        }
        Some(sb)
    }

    /// Read + validate the geometry header from the zone-map area.
    fn read_geometry(device: &dyn BlockDevice) -> Result<StoreGeometry, StoreError> {
        let offset = SB_COPY_SIZE * SB_COPIES;
        let raw = device.read(offset, 4096)?;
        if raw.len() < 8 {
            return Err(StoreError::NoValidSuperblock);
        }
        let len = u32::from_be_bytes(raw[0..4].try_into().unwrap()) as usize;
        let crc = u32::from_be_bytes(raw[4..8].try_into().unwrap());
        if 8 + len > raw.len() {
            return Err(StoreError::NoValidSuperblock);
        }
        if crc32fast::hash(&raw[8..8 + len]) != crc {
            return Err(StoreError::NoValidSuperblock);
        }
        let geo: StoreGeometry =
            bincode::deserialize(&raw[8..8 + len]).map_err(|_| StoreError::NoValidSuperblock)?;
        if geo.zone_count == 0 || geo.zone_size == 0 || geo.segment_size == 0 {
            return Err(StoreError::NoValidSuperblock);
        }
        Ok(geo)
    }

    fn write_geometry(&mut self) -> Result<(), StoreError> {
        let body = bincode::serialize(&self.geo)?;
        let mut raw = Vec::with_capacity(8 + body.len());
        raw.extend_from_slice(&(body.len() as u32).to_be_bytes());
        raw.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
        raw.extend_from_slice(&body);
        let offset = SB_COPY_SIZE * SB_COPIES;
        self.device.write(offset, &raw)?;
        Ok(())
    }

    fn write_sb_copy(device: &mut dyn BlockDevice, index: u64, sb: &Superblock) -> Result<(), StoreError> {
        let body = bincode::serialize(sb)?;
        let mut raw = Vec::with_capacity(16 + body.len());
        raw.extend_from_slice(&(body.len() as u32).to_be_bytes());
        raw.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
        raw.extend_from_slice(&sb.epoch.to_be_bytes());
        raw.extend_from_slice(&body);
        device.write(index * SB_COPY_SIZE, &raw)?;
        Ok(())
    }

    fn write_superblock(
        &mut self,
        root: Hash256,
        checkpoint: CheckpointRef,
    ) -> Result<(), StoreError> {
        self.epoch += 1;
        self.root = root;
        self.checkpoint = checkpoint;
        let sb = Superblock {
            magic: SB_MAGIC,
            version: 1,
            epoch: self.epoch,
            root,
            checkpoint,
        };
        for i in 0..SB_COPIES {
            Self::write_sb_copy(self.device.as_mut(), i, &sb)?;
        }
        self.device.sync()?;
        Ok(())
    }

    /// Write the checkpoint blob into the dedicated area, framed with
    /// length + CRC (torn writes detected at load).
    fn write_checkpoint_blob(&mut self, blob: &[u8]) -> Result<CheckpointRef, StoreError> {
        let offset = self.geo.checkpoint_offset();
        let mut framed = Vec::with_capacity(8 + blob.len());
        framed.extend_from_slice(&(blob.len() as u32).to_be_bytes());
        framed.extend_from_slice(&crc32fast::hash(blob).to_be_bytes());
        framed.extend_from_slice(blob);
        self.device.write(offset, &framed)?;
        Ok(CheckpointRef {
            offset,
            len: blob.len() as u32,
            crc: crc32fast::hash(blob),
        })
    }

    pub fn geometry(&self) -> StoreGeometry {
        self.geo
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn root(&self) -> Hash256 {
        self.root
    }

    /// Persist a checkpoint: the caller serializes engine state (indices,
    /// snapshot table, refcounts...) into `checkpoint_blob`; the store
    /// writes it into the checkpoint area, then atomically-ish updates the
    /// triple-mirrored superblock (epoch bump + CRC + sync).
    pub fn checkpoint(&mut self, root: Hash256, checkpoint_blob: Vec<u8>) -> Result<(), StoreError> {
        let ckpt = self.write_checkpoint_blob(&checkpoint_blob)?;
        self.write_superblock(root, ckpt)
    }

    /// (root, checkpoint blob) from the last durable superblock.
    pub fn load_checkpoint(&mut self) -> (Hash256, Vec<u8>) {
        let ckpt = self.checkpoint;
        let blob = if ckpt.len > 0 {
            self.device
                .read(ckpt.offset + 8, ckpt.len as u64)
                .ok()
                .filter(|b| crc32fast::hash(b) == ckpt.crc)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        (self.root, blob)
    }

    /// Force device-level durability of all volatile writes without
    /// changing the checkpoint payload (epoch bump + sync).
    pub fn checkpoint_noop(&mut self, root: Hash256) -> Result<(), StoreError> {
        let ckpt = self.checkpoint;
        self.write_superblock(root, ckpt)
    }

    // -- extent IO -----------------------------------------------------------

    /// Append `data` into the store, returning its extent descriptor.
    /// Optionally compress first (spec §6 step 7).
    pub fn write_extent(
        &mut self,
        data: &[u8],
        hash: &Hash256,
        compress: bool,
    ) -> Result<ExtentDescriptor, StoreError> {
        let (payload, comp) = if compress {
            match zstd::encode_all(data, 3) {
                Ok(z) if z.len() < data.len() => (z, CompressionType::Zstd),
                _ => (data.to_vec(), CompressionType::Store),
            }
        } else {
            (data.to_vec(), CompressionType::Store)
        };

        // Allocation: reclaimed free-list ranges first (spec §8.2 — cleaned
        // space is reused before fresh zone capacity), then the round-robin
        // cursor from a rotating base. In both paths an extent must never
        // straddle a segment boundary: the cleaner frees whole-segment
        // ranges against segment-granular bookkeeping, and a straddling
        // extent's tail would be attributed to the *next* segment's range.
        // Cost: at most one extent's tail padding per segment (<1%).
        let payload_len = payload.len() as u32;
        let seg_size = self.geo.segment_size;
        let fits_one_segment =
            |off: u32, len: u32| off / seg_size == (off + len - 1) / seg_size;
        let mut free_hit: Option<(u32, usize, u32)> = None; // (zone, range_idx, offset)
        'outer: for (zone, list) in self.free_lists.iter().enumerate() {
            for (ri, &(off, len)) in list.iter().enumerate() {
                if len >= payload_len && fits_one_segment(off, payload_len) {
                    free_hit = Some((zone as u32, ri, off));
                    break 'outer;
                }
            }
        }

        let (zone, offset_in_zone) = if let Some((zone, ri, off)) = free_hit {
            // Carve the allocation out of the freed range; the tail of the
            // range (if any) stays available for smaller extents.
            let (_, len) = self.free_lists[zone as usize][ri];
            let remaining = len - payload_len;
            if remaining == 0 {
                self.free_lists[zone as usize].remove(ri);
            } else {
                self.free_lists[zone as usize][ri] = (off + payload_len, remaining);
            }
            (zone, off)
        } else {
            let mut chosen: Option<u32> = None;
            for i in 0..self.geo.zone_count {
                let zone = (self.epoch.wrapping_add(i as u64) % self.geo.zone_count as u64) as u32;
                let mut cursor = self.zone_cursors[zone as usize];
                if cursor + payload_len as u64 > self.geo.zone_size {
                    continue;
                }
                // Pad to the next segment boundary if the extent would
                // straddle one (the padding bytes are simply not handed out).
                if !fits_one_segment(cursor as u32, payload_len) {
                    let next_seg = ((cursor / seg_size as u64) + 1) * seg_size as u64;
                    if next_seg + payload_len as u64 > self.geo.zone_size {
                        continue;
                    }
                    cursor = next_seg;
                }
                chosen = Some(zone);
                self.zone_cursors[zone as usize] = cursor + payload_len as u64;
                break;
            }
            let zone = chosen.ok_or(StoreError::Full(payload.len()))?;
            (zone, self.zone_cursors[zone as usize] as u32 - payload_len)
        };

        let base = self.geo.zone_offset(zone)?;
        self.device.write(base + offset_in_zone as u64, &payload)?;

        let mut flags = 0u16;
        if comp == CompressionType::Zstd {
            extent_flags::set(&mut flags, 0); // reserved bit unused here
        }
        let desc = ExtentDescriptor {
            content_hash_prefix: hash.prefix_u64(),
            zone_id: zone,
            offset_in_zone,
            length: payload_len,
            ecc_group_id: 0,
            compression_type: comp as u8,
            flags,
        };

        // Track the extent in the zone's open segment.
        let seg_size = self.geo.segment_size;
        let seg_index = offset_in_zone / seg_size;
        let open = &mut self.open_segments[zone as usize];
        let segment = open.get_or_insert_with(|| SegmentMeta {
            zone_id: zone,
            segment_index: seg_index,
            extents: Vec::new(),
            closed_at_secs: 0,
        });
        if seg_index != segment.segment_index {
            // Segment rotated: close the old one, open a new one.
            let mut old = std::mem::replace(
                segment,
                SegmentMeta {
                    zone_id: zone,
                    segment_index: seg_index,
                    extents: Vec::new(),
                    closed_at_secs: 0,
                },
            );
            old.closed_at_secs = 1; // engine stamps real time
            self.closed_segments.push(old);
        }
        segment.extents.push((desc, *hash));

        Ok(desc)
    }

    /// Read an extent back, decompressing if needed.
    pub fn read_extent(&self, desc: &ExtentDescriptor) -> Result<Vec<u8>, StoreError> {
        let base = self.geo.zone_offset(desc.zone_id)?;
        let raw = self.device.read(base + desc.offset_in_zone as u64, desc.length as u64)?;
        match CompressionType::from_u8(desc.compression_type) {
            CompressionType::Store => Ok(raw),
            CompressionType::Zstd => zstd::decode_all(raw.as_slice())
                .map_err(|e| StoreError::Decompress(e.to_string())),
        }
    }

    /// Integrity check: recompute the content hash.
    pub fn verify_extent(&self, desc: &ExtentDescriptor, hash: &Hash256) -> Result<(), StoreError> {
        let data = self.read_extent(desc)?;
        let computed = Hash256::of(&data);
        if computed != *hash {
            return Err(StoreError::ChecksumMismatch { stored: *hash, computed });
        }
        // Also confirm the 64-bit prefix matches the descriptor (audit M6:
        // prefixes are locators — full-hash verify is authoritative).
        if desc.content_hash_prefix != hash.prefix_u64() {
            return Err(StoreError::ChecksumMismatch { stored: *hash, computed });
        }
        Ok(())
    }

    /// Direct raw access (scrubber/healer path).
    pub fn read_raw(&self, zone: u32, offset: u32, len: u32) -> Result<Vec<u8>, StoreError> {
        let base = self.geo.zone_offset(zone)?;
        Ok(self.device.read(base + offset as u64, len as u64)?)
    }

    pub fn write_raw(&mut self, zone: u32, offset: u32, data: &[u8]) -> Result<(), StoreError> {
        let base = self.geo.zone_offset(zone)?;
        Ok(self.device.write(base + offset as u64, data)?)
    }

    // -- refcounts & cleaner interface ---------------------------------------

    /// Snapshot of everything `Store::open` cannot reconstruct: cursors,
    /// refcounts, closed segments, free lists. Checkpointed by the engine.
    pub fn state(&self) -> StoreState {
        StoreState {
            zone_cursors: self.zone_cursors.clone(),
            closed_segments: self.closed_segments.clone(),
            refcounts: self.refcounts.iter().map(|(h, c)| (*h, *c)).collect(),
            free_lists: self.free_lists.clone(),
        }
    }

    /// Restore persisted allocator state (after `Store::open`). Shapes that
    /// do not match the geometry are rejected — a checkpoint from another
    /// layout must not be applied.
    pub fn restore(&mut self, state: StoreState) -> Result<(), StoreError> {
        if state.zone_cursors.len() != self.geo.zone_count as usize
            || state.free_lists.len() != self.geo.zone_count as usize
        {
            return Err(StoreError::Corrupt("store state does not match geometry".into()));
        }
        self.zone_cursors = state.zone_cursors;
        self.closed_segments = state.closed_segments;
        self.refcounts = state.refcounts.into_iter().collect();
        self.free_lists = state.free_lists;
        Ok(())
    }

    /// Consume the store, returning the device (test reopen support).
    pub fn into_device(self) -> Box<dyn BlockDevice> {
        self.device
    }

    /// Guarantee every cursor sits strictly past `descs`' extents. Used
    /// after a WAL-only recovery (MetadataOnly mode): replay learns extent
    /// locations from `RefOp` records but never appends, so the cursors
    /// must be re-derived or the next write clobbers replayed data.
    pub fn bump_cursors_past(&mut self, descs: &[ExtentDescriptor]) {
        for d in descs {
            let end = d.offset_in_zone as u64 + d.length as u64;
            let z = d.zone_id as usize;
            if z < self.zone_cursors.len() {
                self.zone_cursors[z] = self.zone_cursors[z].max(end);
            }
        }
    }

    /// Bytes currently on the free lists (post-GC reclaimable).
    pub fn free_bytes(&self) -> u64 {
        self.free_lists.iter().flatten().map(|(_, l)| *l as u64).sum()
    }

    pub fn refcount(&self, hash: &Hash256) -> u64 {
        self.refcounts.get(hash).copied().unwrap_or(0)
    }

    pub fn refcount_add(&mut self, hash: Hash256, delta: i64) {
        let entry = self.refcounts.entry(hash).or_insert(0);
        let v = (*entry as i64 + delta).max(0);
        *entry = v as u64;
    }

    /// Cleaner ranking data (spec §8.1): live fraction per closed segment.
    pub fn segment_infos(&self) -> Vec<SegmentInfo> {
        self.closed_segments
            .iter()
            .enumerate()
            .map(|(seg_idx, seg)| {
                let mut live_bytes = 0u64;
                let mut total_bytes = 0u64;
                let mut has_ec_extents = false;
                for (desc, hash) in &seg.extents {
                    total_bytes += desc.length as u64;
                    if desc.ecc_group_id != 0 {
                        has_ec_extents = true;
                    }
                    if self.refcount(hash) > 0 {
                        live_bytes += desc.length as u64;
                    }
                }
                SegmentInfo {
                    seg_idx,
                    zone_id: seg.zone_id,
                    segment_index: seg.segment_index,
                    live_fraction: if total_bytes == 0 {
                        0.0
                    } else {
                        live_bytes as f64 / total_bytes as f64
                    },
                    total_bytes,
                    live_bytes,
                    has_ec_extents,
                }
            })
            .collect()
    }

    /// Clean one closed segment (spec §8.2): rewrite every live extent into
    /// fresh space, then return the whole segment's bytes to the zone's
    /// free list. Returns the move report plus the hash→new-descriptor
    /// mapping the caller (engine) must apply to its indices.
    ///
    /// NOTE the caller contract: the engine tracks EC membership per hash
    /// in its own indices (shards are referenced by hash, not location), so
    /// moved extents keep their stripe linkage there. This store-level
    /// guard only fires for extents whose *recorded* descriptor carries an
    /// EC group id at write time (defense-in-depth for store-level users).
    pub fn clean_segment(
        &mut self,
        seg_idx: usize,
    ) -> Result<(CleanReport, Vec<(Hash256, ExtentDescriptor)>), StoreError> {
        let seg = self
            .closed_segments
            .get(seg_idx)
            .cloned()
            .ok_or(StoreError::ZoneOutOfRange(u32::MAX))?;

        let mut report = CleanReport::default();
        let mut moves = Vec::new();
        for (desc, hash) in &seg.extents {
            if self.refcount(hash) == 0 {
                continue; // dead extent — abandoned, space reclaimed below
            }
            if desc.ecc_group_id != 0 {
                // Conservative: never relocate recorded EC members.
                return Err(StoreError::Corrupt(format!(
                    "segment {} contains EC-stripe extents; cleaner refuses",
                    seg_idx
                )));
            }
            let data = self.read_extent(desc)?;
            let new_desc = self.write_extent(&data, hash, false)?;
            moves.push((*hash, new_desc));
            report.extents_moved += 1;
            report.bytes_moved += desc.length as u64;
        }

        // Return the provably-dead byte ranges of this segment to the zone's
        // free list. Live coverage is computed across ALL segment metas of
        // the zone (open and closed), not just this one: an extent
        // belonging to a neighbouring meta may poke into this segment's
        // range, and freeing its bytes would let a future allocation
        // clobber a live extent. With segment-boundary-respecting
        // allocation (see `write_extent`) straddlers cannot be *created*
        // anymore; this scan keeps the cleaner correct for any extents
        // that predate that rule or bypass it.
        let seg_len = self.geo.segment_size;
        let seg_lo = seg.segment_index as u64 * seg_len as u64;
        let seg_hi = seg_lo + seg_len as u64;
        let z = seg.zone_id as usize;

        let mut live: Vec<(u64, u64)> = Vec::new();
        let mut metas: Vec<&SegmentMeta> = Vec::new();
        if let Some(m) = &self.open_segments[z] {
            metas.push(m);
        }
        for (i, m) in self.closed_segments.iter().enumerate() {
            // Skip this segment itself: its extents were either moved
            // (dead at their old offsets) or already dead.
            if i != seg_idx {
                metas.push(m);
            }
        }
        for meta in metas {
            for (desc, hash) in &meta.extents {
                if self.refcount(hash) == 0 {
                    continue;
                }
                let lo = desc.offset_in_zone as u64;
                let hi = lo + desc.length as u64;
                if hi > seg_lo && lo < seg_hi {
                    live.push((lo.max(seg_lo), hi.min(seg_hi)));
                }
            }
        }
        live.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new(); // disjoint, sorted
        for (lo, hi) in live {
            match merged.last_mut() {
                Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        // Complement of the live coverage = dead ranges to reclaim.
        let mut cursor = seg_lo;
        for (lo, hi) in &merged {
            if *lo > cursor {
                self.free_lists[z].push((cursor as u32, (*lo - cursor) as u32));
                report.bytes_reclaimed += *lo - cursor;
            }
            cursor = cursor.max(*hi);
        }
        if cursor < seg_hi {
            self.free_lists[z].push((cursor as u32, (seg_hi - cursor) as u32));
            report.bytes_reclaimed += seg_hi - cursor;
        }

        // Drop the cleaned segment from the pending list (swap-remove —
        // callers re-fetch `segment_infos()` between calls, never holding
        // seg_idx across a clean).
        self.closed_segments.swap_remove(seg_idx);
        Ok((report, moves))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> StoreGeometry {
        StoreGeometry { zone_count: 4, zone_size: 64 * 1024, segment_size: 16 * 1024 }
    }

    #[test]
    fn extent_roundtrip_with_compression() {
        let mut store = Store::format(Box::new(InMemDevice::new(geometry().total_size())), 4, 64 * 1024, 16 * 1024).unwrap();
        let data = vec![7u8; 10_000]; // highly compressible
        let hash = Hash256::of(&data);
        let desc = store.write_extent(&data, &hash, true).unwrap();
        assert_eq!(desc.compression_type, CompressionType::Zstd as u8);
        let back = store.read_extent(&desc).unwrap();
        assert_eq!(back, data);
        store.verify_extent(&desc, &hash).unwrap();
    }

    #[test]
    fn incompressible_data_falls_back_to_store() {
        let mut store = Store::format(Box::new(InMemDevice::new(geometry().total_size())), 4, 64 * 1024, 16 * 1024).unwrap();
        let mut s = 0x1234_5678_9abc_def0u64;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect();
        let hash = Hash256::of(&data);
        let desc = store.write_extent(&data, &hash, true).unwrap();
        assert_eq!(desc.compression_type, CompressionType::Store as u8);
        assert_eq!(store.read_extent(&desc).unwrap(), data);
    }

    #[test]
    fn corruption_is_detected_by_verify() {
        let mut store = Store::format(Box::new(InMemDevice::new(geometry().total_size())), 4, 64 * 1024, 16 * 1024).unwrap();
        let data = b"integrity matters".to_vec();
        let hash = Hash256::of(&data);
        let desc = store.write_extent(&data, &hash, false).unwrap();
        store.verify_extent(&desc, &hash).unwrap();

        // Silent bit-rot: flip a byte under the extent.
        let base = store.geo.zone_offset(desc.zone_id).unwrap();
        store.device.write(base + desc.offset_in_zone as u64, b"X").unwrap();
        assert!(matches!(
            store.verify_extent(&desc, &hash),
            Err(StoreError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn superblock_majority_survives_single_copy_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev.img");
        let capacity = geometry().total_size();
        let root;
        {
            let mut store = Store::format(
                Box::new(FileDevice::create(&path, capacity).unwrap()),
                4,
                64 * 1024,
                16 * 1024,
            )
            .unwrap();
            root = Hash256::of(b"root-pointer");
            store.checkpoint(root, b"checkpoint-blob".to_vec()).unwrap();
        }
        {
            // Corrupt superblock copy #1 only.
            let mut dev = FileDevice::open(&path, capacity).unwrap();
            dev.write(SB_COPY_SIZE, &[0xFF; 64]).unwrap();
            dev.sync().unwrap();
        }
        {
            let mut store = Store::open(Box::new(FileDevice::open(&path, capacity).unwrap())).unwrap();
            assert_eq!(store.root, root, "majority read must recover the root");
            let (r, blob) = store.load_checkpoint();
            assert_eq!(r, root);
            assert_eq!(blob, b"checkpoint-blob".to_vec());
        }
    }

    #[test]
    fn torn_superblock_write_is_rejected_by_crc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev.img");
        let capacity = geometry().total_size();
        {
            let mut store = Store::format(
                Box::new(FileDevice::create(&path, capacity).unwrap()),
                4,
                64 * 1024,
                16 * 1024,
            )
            .unwrap();
            store.checkpoint(Hash256::of(b"v1"), vec![1]).unwrap();
        }
        {
            // Torn write on ALL three copies → no valid superblock.
            let mut dev = FileDevice::open(&path, capacity).unwrap();
            for i in 0..3u64 {
                dev.write(i * SB_COPY_SIZE + 20, &[0xAB; 8]).unwrap();
            }
            dev.sync().unwrap();
        }
        let result = Store::open(Box::new(FileDevice::open(&path, capacity).unwrap()));
        assert!(matches!(result, Err(StoreError::NoValidSuperblock)));
    }

    #[test]
    fn file_device_power_cut_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev.img");
        let capacity = 4096u64;
        {
            let mut dev = FileDevice::create(&path, capacity).unwrap();
            dev.write(0, b"durable-write").unwrap();
            dev.sync().unwrap();
            dev.write(100, b"volatile-write").unwrap();
            // NO sync → simulated power cut on drop.
        }
        {
            let dev = FileDevice::open(&path, capacity).unwrap();
            let durable = dev.read(0, 13).unwrap();
            assert_eq!(&durable, b"durable-write");
            let lost = dev.read(100, 15).unwrap();
            assert_eq!(&lost, &[0u8; 15], "volatile write must be gone after power cut");
        }
    }

    #[test]
    fn zone_rotation_and_segment_tracking() {
        let geo = StoreGeometry { zone_count: 2, zone_size: 8 * 1024, segment_size: 1024 };
        let mut store =
            Store::format(Box::new(InMemDevice::new(geo.total_size())), 2, 8 * 1024, 1024).unwrap();
        // Write enough extents to force segment closes.
        for i in 0..40u32 {
            let data = vec![i as u8; 256];
            let hash = Hash256::of(&data);
            let desc = store.write_extent(&data, &hash, false).unwrap();
            assert!(desc.offset_in_zone < 8 * 1024);
        }
        // Some segments must have been closed (40 × 256B = 10 KiB > 1 KiB seg).
        assert!(!store.closed_segments.is_empty());
    }

    #[test]
    fn refcount_add_clamps_at_zero() {
        let mut store =
            Store::format(Box::new(InMemDevice::new(geometry().total_size())), 4, 64 * 1024, 16 * 1024).unwrap();
        let h = Hash256::of(b"x");
        store.refcount_add(h, 2);
        assert_eq!(store.refcount(&h), 2);
        store.refcount_add(h, -5);
        assert_eq!(store.refcount(&h), 0, "refcounts clamp at zero");
    }

    #[test]
    fn reopen_preserves_zone_cursors_and_refcounts() {
        // Regression (v1.1 audit pass): Store::open used to reset zone
        // cursors to zero. Post-reopen writes then landed at zone offset 0
        // and silently overwrote live extents; refcounts also vanished,
        // which would have made the cleaner free live data.
        let dev_len = geometry().total_size();
        let mut store =
            Store::format(Box::new(InMemDevice::new(dev_len)), 4, 64 * 1024, 16 * 1024).unwrap();
        // 32 KiB extent: straddles 16 KiB segments, so the allocator pads
        // to the first boundary — zone 1 (epoch rotation), offset 16 KiB.
        let a = vec![0xABu8; 32 * 1024];
        let ha = Hash256::of(&a);
        let b = vec![0xCDu8; 4 * 1024];
        let hb = Hash256::of(&b);
        let da = store.write_extent(&a, &ha, false).unwrap();
        let db = store.write_extent(&b, &hb, false).unwrap();
        store.refcount_add(ha, 1);
        store.refcount_add(hb, 3);
        assert_eq!((da.zone_id, da.offset_in_zone), (1, 16 * 1024), "padded past seg0");
        assert_eq!((db.zone_id, db.offset_in_zone), (1, 48 * 1024), "appends at the cursor");

        // Simulate remount: same device contents, fresh Store::open.
        let state = store.state();
        let device = store.into_device();
        let mut reopened = Store::open(device).unwrap();
        reopened.restore(state).unwrap();

        assert_eq!(reopened.refcount(&ha), 1);
        assert_eq!(reopened.refcount(&hb), 3);

        // A new extent must append AFTER the live data, not over it.
        let c = vec![0xEFu8; 1024];
        let hc = Hash256::of(&c);
        let dc = reopened.write_extent(&c, &hc, false).unwrap();
        assert!(
            dc.zone_id != 1 || dc.offset_in_zone >= 52 * 1024,
            "new write clobbered live zone-1 data: zone={} off={}",
            dc.zone_id,
            dc.offset_in_zone
        );

        // And the original bytes are intact.
        assert_eq!(reopened.read_extent(&da).unwrap(), a);
        assert_eq!(reopened.read_extent(&db).unwrap(), b);
    }

    #[test]
    fn cleaner_moves_live_extents_and_reuses_space() {
        // Segment-friendly writes (1 KiB extents, 16 KiB segments) so the
        // rotation actually closes segments: seg0 = 16 live extents,
        // seg1 = 12 dead + 4 live, seg2 open.
        let geo = geometry();
        let mut store =
            Store::format(Box::new(InMemDevice::new(geo.total_size())), 4, 64 * 1024, 16 * 1024).unwrap();
        let mut live: Vec<(Hash256, ExtentDescriptor)> = Vec::new();
        // seg0: 16 live A-extents (distinct content per extent).
        for i in 0..16u8 {
            let mut data = vec![0xA5u8; 1024];
            data[1023] = i;
            let h = Hash256::of(&data);
            let d = store.write_extent(&data, &h, false).unwrap();
            store.refcount_add(h, 1);
            live.push((h, d));
        }
        // seg1: 12 dead extents, then 4 live ones.
        for i in 0..12u8 {
            let mut data = vec![0xD9u8; 1024];
            data[1023] = i;
            let h = Hash256::of(&data);
            store.write_extent(&data, &h, false).unwrap();
            // no refcount: dead on arrival (as if deleted)
        }
        for i in 0..4u8 {
            let mut data = vec![0xC3u8; 1024];
            data[1023] = i;
            let h = Hash256::of(&data);
            let d = store.write_extent(&data, &h, false).unwrap();
            store.refcount_add(h, 1);
            live.push((h, d));
        }

        // One more write rotates seg1 closed (the cleaner only sees
        // closed segments; seg2 stays open holding the filler).
        let filler = vec![0x11u8; 1024];
        let fh = Hash256::of(&filler);
        store.write_extent(&filler, &fh, false).unwrap();
        store.refcount_add(fh, 1);

        // Clean until quiescent (engine gc() pattern).
        loop {
            let mut best: Option<SegmentInfo> = None;
            for info in store.segment_infos() {
                if info.live_fraction < 0.5
                    && best.is_none_or(|b| info.live_fraction < b.live_fraction)
                {
                    best = Some(info);
                }
            }
            let Some(info) = best else { break };
            let (_report, moves) = store.clean_segment(info.seg_idx).unwrap();
            for (h, d) in &moves {
                if let Some(e) = live.iter_mut().find(|(lh, _)| lh == h) {
                    e.1 = *d;
                }
            }
        }

        // Every live extent must read back intact (moved or not).
        for (h, d) in &live {
            let data = store.read_extent(d).unwrap();
            assert_eq!(Hash256::of(&data), *h, "live extent clobbered by the cleaner");
        }
        // Space was reclaimed and is reusable without clobbering anything.
        assert!(store.free_bytes() > 0);
        for i in 0..8u8 {
            let mut data = vec![0x77u8; 1024];
            data[1023] = i;
            let h = Hash256::of(&data);
            let d = store.write_extent(&data, &h, false).unwrap();
            store.refcount_add(h, 1);
            let data2 = store.read_extent(&d).unwrap();
            assert_eq!(Hash256::of(&data2), h, "reuse clobbered a fresh extent");
        }
        for (h, d) in &live {
            let data = store.read_extent(d).unwrap();
            assert_eq!(Hash256::of(&data), *h, "live extent clobbered after reuse");
        }
    }
}