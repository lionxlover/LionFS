//! The LionFS-cluster engine — every subsystem wired together (spec §3, §6, §7).
//!
//! ```text
//! WRITE  (spec §6):
//!   CDC chunks → BLAKE3 per chunk → dedup lookup (domain-scoped)
//!   → miss: encrypt (convergent, domain key) → WAL Data record (Full mode)
//!   → extent store append → dedup insert
//!   → chunk list is itself content-addressed ("meta chunk")
//!   → VersionNode (parent = previous head, vclock bump, ts)
//!   → namespace rebuild up to root (new dir vnodes)
//!   → WAL [VNode, DirMap, RefOp, HeadOp] + Commit  ← durability point
//!   → HEAD swap + Bε-tree path index insert
//!
//! READ  (spec §7):
//!   resolve(path[, t]) → VersionNode → meta chunk → chunk list
//!   → parallel-ish extent fetch → verify BLAKE3 → decrypt → decompress
//!   → concat. On checksum failure: RS-heal on the fly (§11) when EC is on.
//!
//! RECOVERY (spec §18): superblock majority vote → checkpoint load →
//!   WAL replay of committed transactions → mount. No full fsck: the DAG
//!   and WAL make replay sufficient by construction.
//! ```

use crate::cdc::{Chunker, ChunkerConfig};
use crate::core::{
    EngineConfig, ExtentDescriptor, Hash256, NodeKind, Tier, VectorClock,
    VersionNode, WalMode,
};
use crate::dag::{DiffEntry, Namespace};
use crate::dedup::{ChunkLocation, DedupIndex, DomainId};
use crate::ecc::RsCodec;
use crate::store::{BlockDevice, FileDevice, InMemDevice, Store, StoreGeometry};
use crate::wal::{Wal, WalRecord};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("dag: {0}")]
    Dag(#[from] crate::dag::DagError),
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("wal: {0}")]
    Wal(#[from] crate::wal::WalError),
    #[error("cdc: {0}")]
    Cdc(#[from] crate::cdc::CdcError),
    #[error("crypto: {0}")]
    Crypto(#[from] crate::crypto::CryptoError),
    #[error("dedup: {0}")]
    Dedup(#[from] crate::dedup::DedupError),
    #[error("ecc: {0}")]
    Ecc(#[from] crate::ecc::EccError),
    #[error("serialization: {0}")]
    Codec(#[from] bincode::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("corruption: {0}")]
    Corruption(String),
    #[error("{0}")]
    Other(String),
}

pub type EngineResult<T> = Result<T, EngineError>;

/// Re-export for CLI/tooling convenience.
pub use crate::dag::Change as DiffChange;

/// Telemetry snapshot.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct EngineStats {
    pub files_written: u64,
    pub bytes_logical: u64,
    pub bytes_physical: u64,
    pub chunk_hits: u64,
    pub chunk_misses: u64,
    pub wal_records: u64,
    pub wal_fsyncs: u64,
    pub scrubs: u64,
    pub heals: u64,
}

impl EngineStats {
    /// End-to-end write amplification (logical vs device bytes).
    pub fn write_amplification(&self) -> f64 {
        if self.bytes_physical == 0 {
            1.0
        } else {
            self.bytes_physical as f64 / self.bytes_logical as f64
        }
    }
}

/// A garbage-collection pass report (spec §8).
#[derive(Clone, Copy, Debug, Default)]
pub struct GcReport {
    /// Closed segments whose live fraction fell below the threshold.
    pub segments_cleaned: usize,
    /// Live extents rewritten into fresh space.
    pub extents_moved: usize,
    /// Live bytes rewritten (the GC's own write amplification).
    pub bytes_moved: u64,
    /// Segment bytes returned to the allocator's free lists.
    pub bytes_reclaimed: u64,
    /// Reclaimed bytes currently available for reuse after the pass.
    pub free_bytes_now: u64,
}

/// A scrub pass report (spec §11).
#[derive(Clone, Debug, Default)]
pub struct ScrubReport {
    pub extents_checked: u64,
    pub corruptions_found: u64,
    pub healed: u64,
    pub unhealable: u64,
}

/// ERASURE-CODED group bookkeeping (spec §5.3).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct EcGroup {
    /// k data hashes + m parity hashes.
    shards: Vec<Hash256>,
    shard_len: usize,
}

/// A stored chunk: the on-device bytes are compress+encrypt(chunk), whose
/// hash (`stored`) differs from the plaintext identity (`chunk_map` key).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredChunk {
    desc: ExtentDescriptor,
    /// Hash of the exact on-device bytes (integrity check for the raw
    /// extent; the plaintext hash is verified after decrypt).
    stored: Hash256,
}

/// Advisory mount lock (LionFS 8.0 fix for the concurrent-mount race).
///
/// v8.0 regression fix: two `mount()` calls racing on the same directory
/// could interleave their recovery checkpoints — `write_checkpoint_blob`
/// writes the blob **in place** at a fixed offset, so the loser's write
/// tears the winner's bytes; the next `load_checkpoint` then sees a CRC
/// mismatch and silently falls back to an empty namespace, surfacing as
/// `Dag(PathNotFound)` and, in production, as silent data loss. The WAL
/// fallback itself is crash-safe by design, so the fix is exclusion: an
/// exclusive advisory lock on `<dir>/mount.lock`, held across the whole
/// mount/create critical section (format or checkpoint-load → WAL replay
/// → recovery checkpoint) and released when the engine is handed to the
/// caller. Long-lived exclusivity between *writers* is the deployment's
/// contract (single mounter per directory; cluster consensus governs
/// multi-node), matching how real filesystems refuse double mounts.
struct MountGuard {
    _file: std::fs::File,
}

impl MountGuard {
    /// Acquire the blocking exclusive mount lock for `dir`.
    /// Uses std file locking (stable since Rust 1.89; the cluster plane's
    /// MSRV is 1.89 for this reason, the local engine keeps 1.75).
    fn acquire(dir: &Path) -> EngineResult<Self> {
        let path = dir.join("mount.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        std::fs::File::lock(&file)?;
        Ok(MountGuard { _file: file })
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        // Dropping the File releases the OS lock; the explicit unlock is
        // belt-and-braces and ignores errors deliberately.
        let _ = self._file.unlock();
    }
}


/// The engine.
pub struct ClusterEngine {
    pub config: EngineConfig,
    namespace: Namespace,
    store: Store,
    wal: Wal,
    dedup: DedupIndex,
    chunker: Chunker,
    keytree: crate::crypto::KeyTree,
    domain_key: [u8; 32],
    domain: DomainId,
    cipher: crate::crypto::Cipher,
    /// plaintext hash → stored chunk (descriptor + on-device-bytes hash).
    chunk_map: HashMap<Hash256, StoredChunk>,
    /// EC groups: group id → members.
    ec_groups: HashMap<u8, EcGroup>,
    next_ec_group: u8,
    ec: Option<RsCodec>,
    /// Path index (spec §3 metadata engine): path → current vnode hash.
    path_index: crate::btree::BeTree<Vec<u8>>,
    vclock: VectorClock,
    /// Hybrid Logical Clock (audit C3): anchors `timestamp_ns`, strictly
    /// monotone locally and cuttable across a future cluster deployment.
    hlc: crate::core::Hlc,
    node_id: crate::core::NodeId,
    pub stats: EngineStats,
    wal_path: PathBuf,
    device_path: Option<PathBuf>,
}

// Geometry used by the prototype (small so tests are fast; production uses
// spec §4's 128 MiB segments / 16 MiB zones).
const GEO_ZONES: u32 = 8;
const GEO_ZONE_SIZE: u64 = 4 * 1024 * 1024;
const GEO_SEGMENT: u32 = 512 * 1024;

/// Total device capacity for the prototype geometry.
fn device_capacity() -> u64 {
    let geo = StoreGeometry {
        zone_count: GEO_ZONES,
        zone_size: GEO_ZONE_SIZE,
        segment_size: GEO_SEGMENT,
    };
    geo.total_size()
}

impl ClusterEngine {
    // -- lifecycle ------------------------------------------------------------

    /// Create a fresh filesystem inside `dir` (device + WAL + key file).
    ///
    /// The `master.key` file is the prototype stand-in for the HSM/TPM-sealed
    /// root key of spec §15: an out-of-band secret the FS needs to operate
    /// but does not manage itself.
    pub fn create(dir: &Path, config: EngineConfig) -> EngineResult<Self> {
        std::fs::create_dir_all(dir)?;
        let _mount_guard = MountGuard::acquire(dir)?;
        let device_path = dir.join("device.img");
        let wal_path = dir.join("wal.log");
        let key_path = dir.join("master.key");
        let capacity = device_capacity();
        let device = Box::new(FileDevice::create(&device_path, capacity)?);
        let engine = Self::build(device, &wal_path, Some(device_path), config)?;
        std::fs::write(&key_path, engine.keytree.master())?;
        Ok(engine)
    }

    /// In-memory device + file WAL in `dir` (fast tests with real WAL).
    pub fn create_mem_in(dir: &Path, config: EngineConfig) -> EngineResult<Self> {
        std::fs::create_dir_all(dir)?;
        let _mount_guard = MountGuard::acquire(dir)?;
        let wal_path = dir.join("wal.log");
        let capacity = device_capacity();
        let device = Box::new(InMemDevice::new(capacity));
        Self::build(device, &wal_path, None, config)
    }

    fn build(
        device: Box<dyn BlockDevice>,
        wal_path: &Path,
        device_path: Option<PathBuf>,
        config: EngineConfig,
    ) -> EngineResult<Self> {
        let store = Store::format(device, GEO_ZONES, GEO_ZONE_SIZE, GEO_SEGMENT)?;
        let wal = Wal::create(wal_path)?;
        let keytree = crate::crypto::KeyTree::generate();
        let domain = DomainId::root();
        let domain_key = keytree.domain_key(&domain.0);
        let ec = if config.ec.m > 0 {
            Some(RsCodec::new(config.ec.k as usize, config.ec.m as usize)?)
        } else {
            None
        };
        let chunker = Chunker::new(ChunkerConfig {
            avg: config.chunk_avg_bytes,
            min: config.chunk_min_bytes,
            max: config.chunk_max_bytes,
        })?;
        let namespace = Namespace::new(1, 1_000_000_000)?;
        let mut vclock = VectorClock::new();
        vclock.increment(1);
        Ok(Self {
            config,
            namespace,
            store,
            wal,
            dedup: DedupIndex::new(),
            chunker,
            keytree,
            domain_key,
            domain,
            cipher: crate::crypto::Cipher::Aes256Gcm,
            chunk_map: HashMap::new(),
            ec_groups: HashMap::new(),
            next_ec_group: 0,
            ec,
            path_index: crate::btree::BeTree::new(crate::btree::TreeConfig {
                max_children: 8,
                buffer_capacity: 32,
                leaf_max_entries: 32,
            }),
            vclock,
            hlc: crate::core::Hlc::EPOCH,
            node_id: 1,
            stats: EngineStats::default(),
            wal_path: wal_path.to_path_buf(),
            device_path,
        })
    }

    /// Mount an existing filesystem (spec §18 Mount()).
    ///
    /// Steps:
    ///
    /// 1. superblock majority vote
    /// 2. checkpoint load
    /// 3. WAL replay of committed transactions
    /// 4. ready — no full fsck
    pub fn mount(dir: &Path, config: EngineConfig) -> EngineResult<Self> {
        std::fs::create_dir_all(dir)?;
        // v8.0 fix: serialize the recovery critical section against
        // concurrent mounts (see MountGuard). Held through the recovery
        // checkpoint below, released when this function returns.
        let _mount_guard = MountGuard::acquire(dir)?;
        let device_path = dir.join("device.img");
        let wal_path = dir.join("wal.log");
        let key_path = dir.join("master.key");
        let capacity = device_capacity();
        let device = Box::new(FileDevice::open(&device_path, capacity)?);
        let mut store = Store::open(device)?;
        let master_key: [u8; 32] = std::fs::read(&key_path)
            .map_err(|e| EngineError::Other(format!("master.key (HSM stand-in) missing: {e}")))?
            .try_into()
            .map_err(|_| EngineError::Corruption("master.key must be 32 bytes".into()))?;

        let ec = if config.ec.m > 0 {
            Some(RsCodec::new(config.ec.k as usize, config.ec.m as usize)?)
        } else {
            None
        };
        let chunker = Chunker::new(ChunkerConfig {
            avg: config.chunk_avg_bytes,
            min: config.chunk_min_bytes,
            max: config.chunk_max_bytes,
        })?;

        // Try to load a checkpoint.
        let (_, blob) = store.load_checkpoint();
        let mut engine = if !blob.is_empty() {
            match Self::decode_checkpoint(&blob) {
                Some(state) => Self::from_state(state, store, &wal_path, Some(device_path), config)?,
                None => return Err(EngineError::Corruption("checkpoint undecodable".into())),
            }
        } else {
            // No checkpoint: build empty then replay everything from the WAL,
            // with the out-of-band master key (HSM stand-in).
            let namespace = Namespace::new(1, 1_000_000_000)?;
            let mut vclock = VectorClock::new();
            vclock.increment(1);
            let keytree = crate::crypto::KeyTree::from_master(master_key);
            let domain = DomainId::root();
            let domain_key = keytree.domain_key(&domain.0);
            let wal = Wal::open(&wal_path)?;
            Self {
                config: config.clone(),
                namespace,
                store,
                wal,
                dedup: DedupIndex::new(),
                chunker,
                keytree,
                domain_key,
                domain,
                cipher: crate::crypto::Cipher::Aes256Gcm,
                chunk_map: HashMap::new(),
                ec_groups: HashMap::new(),
                next_ec_group: 0,
                ec,
                path_index: crate::btree::BeTree::new(crate::btree::TreeConfig {
                    max_children: 8,
                    buffer_capacity: 32,
                    leaf_max_entries: 32,
                }),
                vclock,
                hlc: crate::core::Hlc::EPOCH,
                node_id: 1,
                stats: EngineStats::default(),
                wal_path: wal_path.clone(),
                device_path: Some(device_path),
            }
        };

        // 3. WAL replay (spec §18 steps 3-8)...
        let recovered_from_wal = engine.replay_wal()?;
        // 3b. WAL-only recovery (no usable checkpoint) learned extent
        // locations from RefOp records without appending — re-derive the
        // allocation cursors so the next write cannot clobber replayed
        // extents (v1.1 regression fix). It also bypassed the live
        // put/delete path, so the Bε-tree path index must be rebuilt by
        // walking the recovered namespace.
        if recovered_from_wal {
            // The path index is only mutated on the live put/delete path;
            // replay reconstructed the namespace without it. The walk is
            // O(entries) — production journals path ops instead.
            engine.rebuild_path_index()?;
            let descs: Vec<ExtentDescriptor> =
                engine.chunk_map.values().map(|sc| sc.desc).collect();
            engine.store.bump_cursors_past(&descs);
        }
        // 4. ...immediately followed by a checkpoint (crash-safe recovery:
        //    dying between replay and checkpoint replays the same WAL).
        engine.checkpoint()?;
        // 5. Mount READY — no full fsck (spec §18 step 11).
        Ok(engine)
    }

    fn decode_checkpoint(blob: &[u8]) -> Option<CheckpointState> {
        bincode::deserialize(blob).ok()
    }

    fn from_state(
        state: CheckpointState,
        mut store: Store,
        wal_path: &Path,
        device_path: Option<PathBuf>,
        config: EngineConfig,
    ) -> EngineResult<Self> {
        let ec = if config.ec.m > 0 {
            Some(RsCodec::new(config.ec.k as usize, config.ec.m as usize)?)
        } else {
            None
        };
        let chunker = Chunker::new(ChunkerConfig {
            avg: config.chunk_avg_bytes,
            min: config.chunk_min_bytes,
            max: config.chunk_max_bytes,
        })?;
        let wal = Wal::open(wal_path)?;
        let keytree = crate::crypto::KeyTree::from_master(state.master_key);
        let domain = DomainId(state.domain);
        let domain_key = keytree.domain_key(&domain.0);
        store.restore(state.store_state)?;
        Ok(Self {
            config,
            namespace: crate::dag::Namespace::from_state(state.namespace)?,
            store,
            wal,
            dedup: crate::dedup::DedupIndex::from_state(state.dedup),
            chunker,
            keytree,
            domain_key,
            domain,
            cipher: state.cipher,
            chunk_map: state.chunk_map,
            ec_groups: state.ec_groups,
            next_ec_group: state.next_ec_group,
            ec,
            path_index: state.path_index,
            vclock: state.vclock,
            hlc: state.hlc,
            node_id: state.node_id,
            stats: state.stats,
            wal_path: wal_path.to_path_buf(),
            device_path,
        })
    }

    /// Serialize + persist a checkpoint: superblock (epoch bump, CRC × 3,
    /// sync) + WAL reset (spec §18 "since last checkpoint").
    pub fn checkpoint(&mut self) -> EngineResult<()> {
        self.path_index.flush_all();
        let state = CheckpointState {
            namespace: self.namespace.clone_state(),
            dedup: self.dedup.clone_state(),
            chunk_map: self.chunk_map.clone(),
            ec_groups: self.ec_groups.clone(),
            next_ec_group: self.next_ec_group,
            path_index: self.path_index.clone(),
            vclock: self.vclock.clone(),
            hlc: self.hlc,
            node_id: self.node_id,
            master_key: *self.keytree.master(),
            domain: self.domain.0.clone(),
            cipher: self.cipher,
            stats: self.stats.clone(),
            store_state: self.store.state(),
        };
        let blob = bincode::serialize(&state)?;
        let root = self.namespace.root();
        self.store.checkpoint(root, blob)?;
        self.wal.reset()?;
        Ok(())
    }

    /// WAL replay (spec §18): apply every committed transaction in order;
    /// trailing uncommitted data is discarded by the WAL layer. The caller
    /// (mount) checkpoints immediately after, making recovery idempotent —
    /// if the process dies mid-recovery, the next mount replays the same
    /// WAL against the same checkpoint state.
    /// Replay committed transactions. Returns true when any record was
    /// applied (i.e. this was a WAL-only recovery — the checkpoint was
    /// missing or stale).
    fn replay_wal(&mut self) -> EngineResult<bool> {
        let txns = Wal::replay(&self.wal_path)?;
        let mut applied = false;
        for (_txn, records) in txns {
            for record in records {
                self.apply_wal_record(record)?;
                applied = true;
            }
        }
        Ok(applied)
    }

    fn apply_wal_record(&mut self, record: WalRecord) -> EngineResult<()> {
        match record {
            WalRecord::Data { hash, payload } => {
                if !self.chunk_map.contains_key(&hash) {
                    let stored_hash = Hash256::of(&payload);
                    // The hash param is the extent's *identity* — the
                    // refcount key the cleaner resolves (integrity uses
                    // `stored`, kept separately in StoredChunk).
                    let desc = self.store.write_extent(&payload, &hash, false)?;
                    self.chunk_map.insert(hash, StoredChunk { desc, stored: stored_hash });
                }
            }
            WalRecord::VNode { bytes } => {
                if let Some(node) = VersionNode::from_bytes(&bytes) {
                    self.namespace.repo.put(node)?;
                }
            }
            WalRecord::DirMap { content, children } => {
                self.namespace.register_dir(content, children.into_iter().collect());
            }
            WalRecord::RefOp { hash, delta, zone, offset, length, stored } => {
                self.chunk_map.entry(hash).or_insert_with(|| StoredChunk {
                            desc: ExtentDescriptor {
                                content_hash_prefix: hash.prefix_u64(),
                                zone_id: zone,
                                offset_in_zone: offset,
                                length,
                                ecc_group_id: 0,
                                compression_type: 0,
                                flags: 0,
                            },
                            stored,
                        });
                self.store.refcount_add(hash, delta);
                if delta > 0 {
                    self.dedup.replay_acquire(&self.domain, &hash);
                } else if delta < 0 {
                    self.dedup.replay_release(&self.domain, &hash);
                }
            }
            WalRecord::HeadOp { root, ts } => {
                self.namespace.set_root_from_recovery(root, ts);
            }
            WalRecord::SnapshotOp { name, root } => {
                self.namespace.snapshot_from_recovery(name, root);
            }
            WalRecord::Commit { .. } => {}
        }
        Ok(())
    }

    // -- clock ---------------------------------------------------------------

    /// Deterministic floor for timestamps (1 s after epoch): a machine with
    /// a broken/pre-epoch clock still gets sane, ordered timestamps.
    const MIN_TS_NS: u64 = 1_000_000_000;

    fn now(&mut self) -> u64 {
        // HLC tick (audit C3): the packed form is strictly monotone, so
        // `resolve(path, t)` cuts are well-defined even when the wall clock
        // stands still between two writes.
        let wall = wall_ns_now().max(Self::MIN_TS_NS);
        self.hlc.tick_ns(wall)
    }

    // -- write path (spec §6) --------------------------------------------------

    /// Write (or overwrite) a file. Returns the new version's hash.
    pub fn put(&mut self, path: &str, data: &[u8]) -> EngineResult<Hash256> {
        let ts = self.now();
        let mut wal_records: Vec<WalRecord> = Vec::new();

        // 1. Content-defined chunks (spec §6 step 1, §10.1).
        let chunks = self.chunker.chunk(data);
        let mut chunk_hashes: Vec<Hash256> = Vec::with_capacity(chunks.len());

        for chunk in chunks {
            let h = Hash256::of(chunk);
            chunk_hashes.push(h);

            // 2-5. Dedup (domain-scoped).
            if let Some(_loc) = self.dedup.lookup(&self.domain, &h) {
                self.stats.chunk_hits += 1;
                self.store.refcount_add(h, 1);
                let sc = self.chunk_map[&h].clone();
                wal_records.push(WalRecord::RefOp {
                    hash: h,
                    delta: 1,
                    zone: sc.desc.zone_id,
                    offset: sc.desc.offset_in_zone,
                    length: sc.desc.length,
                    stored: sc.stored,
                });
                self.dedup.account_duplicate(chunk.len() as u64);
                continue;
            }
            self.stats.chunk_misses += 1;

            // 6-8. Compress + encrypt (convergent within the domain).
            let compressed = zstd_compress(chunk);
            let payload = self.keytree.encrypt_extent(
                &self.domain_key,
                &h,
                &compressed,
                self.cipher,
            )?;

            // 9. WAL the payload (Full mode — spec §6 step 9; audit C1
            //    documents the 2× cost and the MetadataOnly alternative).
            if self.config.wal_mode == WalMode::Full {
                wal_records.push(WalRecord::Data { hash: h, payload: payload.clone() });
            }

            // 10. Stage into the extent store. The on-device bytes hash
            // (`stored`) differs from the plaintext identity — the extent
            // descriptor carries the locator, integrity uses `stored`.
            let stored_hash = Hash256::of(&payload);
            // Identity (refcount key) = plaintext hash; integrity hash is
            // kept separately. M6: the descriptor prefix is a locator.
            let desc = self.store.write_extent(&payload, &h, false)?;
            self.chunk_map.insert(h, StoredChunk { desc, stored: stored_hash });
            self.store.refcount_add(h, 1);
            self.dedup.insert_new(
                &self.domain,
                &h,
                ChunkLocation {
                    zone_id: desc.zone_id,
                    offset_in_zone: desc.offset_in_zone,
                    length: desc.length,
                },
                chunk.len() as u64,
            );
            self.stats.bytes_physical += payload.len() as u64;
            wal_records.push(WalRecord::RefOp {
                hash: h,
                delta: 1,
                zone: desc.zone_id,
                offset: desc.offset_in_zone,
                length: desc.length,
                stored: stored_hash,
            });
        }

        // The chunk LIST is itself content-addressed ("meta chunk").
        let meta = bincode::serialize(&chunk_hashes)?;
        let meta_hash = Hash256::of(&meta);
        if !self.chunk_map.contains_key(&meta_hash) {
            let meta_compressed = zstd_compress(&meta);
            let meta_payload = self
                .keytree
                .encrypt_extent(&self.domain_key, &meta_hash, &meta_compressed, self.cipher)?;
            let meta_stored = Hash256::of(&meta_payload);
            if self.config.wal_mode == WalMode::Full {
                wal_records.push(WalRecord::Data { hash: meta_hash, payload: meta_payload.clone() });
            }
            let desc = self.store.write_extent(&meta_payload, &meta_hash, false)?;
            self.chunk_map.insert(meta_hash, StoredChunk { desc, stored: meta_stored });
            self.store.refcount_add(meta_hash, 1);
            self.dedup.insert_new(
                &self.domain,
                &meta_hash,
                ChunkLocation {
                    zone_id: desc.zone_id,
                    offset_in_zone: desc.offset_in_zone,
                    length: desc.length,
                },
                meta.len() as u64,
            );
            wal_records.push(WalRecord::RefOp {
                hash: meta_hash,
                delta: 1,
                zone: desc.zone_id,
                offset: desc.offset_in_zone,
                length: desc.length,
                stored: meta_stored,
            });
        } else {
            self.store.refcount_add(meta_hash, 1);
            self.dedup.lookup(&self.domain, &meta_hash);
            self.dedup.account_duplicate(meta.len() as u64);
            let sc = self.chunk_map[&meta_hash].clone();
            wal_records.push(WalRecord::RefOp {
                hash: meta_hash,
                delta: 1,
                zone: sc.desc.zone_id,
                offset: sc.desc.offset_in_zone,
                length: sc.desc.length,
                stored: sc.stored,
            });
        }

        // 14-16. New VersionNode (parent = current head, vclock bump).
        let parent = self.namespace.resolve(path).ok();
        self.vclock.increment(self.node_id);
        let node = VersionNode::new(
            meta_hash,
            parent.into_iter().collect(),
            self.vclock.clone(),
            ts,
            NodeKind::File,
            Default::default(),
        );
        let node_hash = self.namespace.repo.put(node.clone())?;
        wal_records.push(WalRecord::VNode { bytes: node.to_bytes() });

        // 12-13, 17. Namespace rebuild up to root; collect created dir nodes.
        let new_root = self.namespace.set_path_at(path, node_hash, ts)?;
        for (vnode, children) in self.namespace.drain_created() {
            wal_records.push(WalRecord::VNode { bytes: vnode.to_bytes() });
            if let Some(map) = children {
                wal_records.push(WalRecord::DirMap {
                    content: vnode.content_hash,
                    children: map.into_iter().collect(),
                });
            }
        }
        // 18. Atomic HEAD swap (durability below).
        wal_records.push(WalRecord::HeadOp { root: new_root, ts });

        // MetadataOnly mode (audit C1): data extents are already in the
        // store — sync the device so data is durable BEFORE the commit
        // marker, then journal metadata only.
        if self.config.wal_mode == WalMode::MetadataOnly {
            self.store_sync();
        }

        // 19. Commit — THE durability point (group commit batches the fsync).
        self.wal.commit_batch(wal_records.clone())?;
        self.stats.wal_records += wal_records.len() as u64;
        self.stats.wal_fsyncs += 1;
        self.stats.files_written += 1;
        self.stats.bytes_logical += data.len() as u64;

        // Bε-tree path index (buffered; spec §3 metadata engine).
        self.path_index.insert(path.as_bytes(), node_hash.0.to_vec());

        Ok(node_hash)
    }

    fn store_sync(&mut self) {
        // Store sync happens through superblock/device writes; the extent
        // writes above already landed in the device's volatile overlay, and
        // MetadataOnly durability comes from the FileDevice sync below —
        // surfaced via a raw sync through a superblock epoch bump.
        let root = self.namespace.root();
        let _ = self.store.checkpoint_noop(root);
    }

    // -- read path (spec §7) ---------------------------------------------------

    /// Read the current version of a file.
    pub fn get(&mut self, path: &str) -> EngineResult<Vec<u8>> {
        self.get_at(path, u64::MAX)
    }

    /// Read a file as of time `t` (ns) — time travel (spec §2.2).
    pub fn get_at(&mut self, path: &str, t: u64) -> EngineResult<Vec<u8>> {
        let head = self.namespace.resolve_at(path, t)?;
        self.read_version(&head)
    }

    /// Read a specific version by hash.
    pub fn read_version(&mut self, head: &Hash256) -> EngineResult<Vec<u8>> {
        let node = self.namespace.repo.get(head)?.clone();
        if node.kind == NodeKind::Tombstone {
            return Err(EngineError::NotFound("deleted".into()));
        }
        // Meta chunk → chunk list.
        let chunk_hashes: Vec<Hash256> = {
            let meta = self.read_chunk(&node.content_hash)?;
            bincode::deserialize(&meta)?
        };
        // Fetch chunks and reassemble (§7 steps 3-9).
        let mut out = Vec::with_capacity(node.meta.permissions as usize);
        for h in chunk_hashes {
            out.extend_from_slice(&self.read_chunk(&h)?);
        }
        Ok(out)
    }

    /// Read one chunk with integrity check + on-the-fly heal (§7 step 5-6).
    fn read_chunk(&mut self, hash: &Hash256) -> EngineResult<Vec<u8>> {
        let sc = self
            .chunk_map
            .get(hash)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(format!("chunk {}", hash.short())))?;

        let payload = match self.fetch_and_verify(&sc) {
            Ok(payload) => payload,
            Err(_) => {
                // Self-heal on the fly via erasure coding (§11).
                
                self.heal_chunk(hash)?
            }
        };
        let plain = self.decrypt_decompress(hash, &payload)?;
        // Final authority: the plaintext identity (audit M6 — prefixes are
        // locators; this is the identity check).
        if Hash256::of(&plain) != *hash {
            return Err(EngineError::Corruption(format!("chunk {}", hash.short())));
        }
        Ok(plain)
    }

    /// Fetch raw stored bytes and verify them against their on-device hash.
    fn fetch_and_verify(&self, sc: &StoredChunk) -> EngineResult<Vec<u8>> {
        let payload = self.store.read_extent(&sc.desc)?;
        if sc.stored != Hash256::zero() && Hash256::of(&payload) != sc.stored {
            return Err(EngineError::Corruption(format!("extent {}", sc.stored.short())));
        }
        Ok(payload)
    }

    fn decrypt_decompress(&mut self, hash: &Hash256, payload: &[u8]) -> EngineResult<Vec<u8>> {
        let compressed = self.keytree.decrypt_extent(&self.domain_key, hash, payload, self.cipher)?;
        let plain = zstd_decompress(&compressed)?;
        Ok(plain)
    }

    /// Erasure-decode a corrupt chunk from its stripe (spec §5.3, §11).
    /// Returns the healed ON-DEVICE bytes (still encrypted/compressed).
    fn heal_chunk(&mut self, hash: &Hash256) -> EngineResult<Vec<u8>> {
        self.stats.heals += 1;
        let sc = self
            .chunk_map
            .get(hash)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(format!("chunk {}", hash.short())))?;
        let group_id = sc.desc.ecc_group_id;
        let group = match self.ec_groups.get(&group_id) {
            Some(g) => g.clone(),
            None => {
                return Err(EngineError::Corruption(format!(
                    "chunk {} corrupt and no EC group to heal from",
                    hash.short()
                )))
            }
        };
        // Gather surviving shards (raw stored bytes, hash-verified).
        let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(group.shards.len());
        for sh in &group.shards {
            let sh_sc = self.chunk_map.get(sh).cloned();
            match sh_sc {
                Some(d) => match self.fetch_and_verify(&d) {
                    Ok(mut p) => {
                        p.resize(group.shard_len, 0);
                        shards.push(Some(p));
                    }
                    Err(_) => shards.push(None),
                },
                None => shards.push(None),
            }
        }
        let codec = self
            .ec
            .as_ref()
            .ok_or_else(|| EngineError::Other("EC disabled".into()))?;
        let recovered = codec.decode(&shards)?;
        let idx = group
            .shards
            .iter()
            .position(|h| h == hash)
            .ok_or_else(|| EngineError::Corruption("shard not in group".into()))?;
        let mut healed = recovered[idx].clone();
        // We padded to shard_len during encode; the true payload length is
        // in the descriptor.
        healed.truncate(sc.desc.length as usize);

        // Rewrite the healed chunk in place (§11 step 9).
        self.store.write_raw(sc.desc.zone_id, sc.desc.offset_in_zone, &healed)?;
        Ok(healed)
    }

    // -- erasure coding (spec §5.3) --------------------------------------------

    /// Encode existing chunks into stripe groups (invoked by the engine in
    /// EC mode after writes; exposed for tests/demo).
    pub fn encode_ec_groups(&mut self) -> EngineResult<usize> {
        let codec = match self.ec.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(0),
        };
        let k = self.config.ec.k as usize;
        let m = self.config.ec.m as usize;
        // Candidate data chunks: those not yet in a group.
        let mut grouped: std::collections::HashSet<Hash256> = Default::default();
        for g in self.ec_groups.values() {
            for h in &g.shards[..k.min(g.shards.len())] {
                grouped.insert(*h);
            }
        }
        let candidates: Vec<Hash256> = self
            .chunk_map
            .keys()
            .filter(|h| !grouped.contains(*h) && self.store.refcount(h) > 0)
            .copied()
            .collect();

        let mut created = 0usize;
        for group_chunks in candidates.chunks(k) {
            if group_chunks.len() < k {
                continue; // partial group stays unencoded (prototype rule)
            }
            let shard_len = group_chunks
                .iter()
                .filter_map(|h| self.chunk_map.get(h).map(|c| c.desc.length as usize))
                .max()
                .unwrap_or(0);
            let mut data_shards: Vec<Vec<u8>> = Vec::with_capacity(k);
            for h in group_chunks {
                let mut shard = self.fetch_and_verify(&self.chunk_map[h])?;
                shard.resize(shard_len, 0);
                data_shards.push(shard);
            }
            let parity = codec.encode(&data_shards)?;
            let group_id = self.next_ec_group;
            self.next_ec_group = self.next_ec_group.wrapping_add(1);
            let mut shards: Vec<Hash256> = Vec::with_capacity(k + m);
            for h in group_chunks {
                let mut stored_chunk = self.chunk_map[h].clone();
                stored_chunk.desc.ecc_group_id = group_id;
                self.chunk_map.insert(*h, stored_chunk);
                shards.push(*h);
            }
            for p in parity {
                let ph = Hash256::of(&p);
                let mut desc = self.store.write_extent(&p, &ph, false)?;
                desc.ecc_group_id = group_id;
                self.chunk_map.insert(ph, StoredChunk { desc, stored: ph });
                // Parity extents are live data too — without a refcount the
                // cleaner would reclaim them and the stripe would silently
                // lose its redundancy.
                self.store.refcount_add(ph, 1);
                shards.push(ph);
            }
            self.ec_groups
                .insert(group_id, EcGroup { shards, shard_len });
            created += 1;
        }
        Ok(created)
    }

    // -- deletion & GC ----------------------------------------------------------

    /// Delete a file (tombstone VersionNode + refcount release).
    pub fn delete(&mut self, path: &str) -> EngineResult<Hash256> {
        let ts = self.now();
        let parent = self.namespace.resolve(path)?;
        let mut wal_records: Vec<WalRecord> = Vec::new();

        // Release chunk refcounts.
        let node = self.namespace.repo.get(&parent)?.clone();
        if node.kind == NodeKind::File {
            let meta: Vec<u8> = self.read_chunk(&node.content_hash)?;
            let chunks: Vec<Hash256> = bincode::deserialize(&meta)?;
            for h in chunks {
                self.store.refcount_add(h, -1);
                if self.dedup.refcount(&self.domain, &h) > 0 {
                    let _ = self.dedup.release(&self.domain, &h);
                }
                let sc = self.chunk_map[&h].clone();
                wal_records.push(WalRecord::RefOp {
                    hash: h,
                    delta: -1,
                    zone: sc.desc.zone_id,
                    offset: sc.desc.offset_in_zone,
                    length: sc.desc.length,
                    stored: sc.stored,
                });
            }
            self.store.refcount_add(node.content_hash, -1);
            if self.dedup.refcount(&self.domain, &node.content_hash) > 0 {
                let _ = self.dedup.release(&self.domain, &node.content_hash);
            }
            let sc = self.chunk_map[&node.content_hash].clone();
            wal_records.push(WalRecord::RefOp {
                hash: node.content_hash,
                delta: -1,
                zone: sc.desc.zone_id,
                offset: sc.desc.offset_in_zone,
                length: sc.desc.length,
                stored: sc.stored,
            });
        }

        self.vclock.increment(self.node_id);
        let tomb = VersionNode::new(
            node.content_hash,
            vec![parent],
            self.vclock.clone(),
            ts,
            NodeKind::Tombstone,
            node.meta.clone(),
        );
        let tomb_hash = self.namespace.repo.put(tomb.clone())?;
        wal_records.push(WalRecord::VNode { bytes: tomb.to_bytes() });
        let new_root = self.namespace.set_path_at(path, tomb_hash, ts)?;
        for (vnode, children) in self.namespace.drain_created() {
            wal_records.push(WalRecord::VNode { bytes: vnode.to_bytes() });
            if let Some(map) = children {
                wal_records.push(WalRecord::DirMap {
                    content: vnode.content_hash,
                    children: map.into_iter().collect(),
                });
            }
        }
        wal_records.push(WalRecord::HeadOp { root: new_root, ts });
        self.wal.commit_batch(wal_records.clone())?;
        self.stats.wal_records += wal_records.len() as u64;
        self.stats.wal_fsyncs += 1;
        self.path_index.delete(path.as_bytes());
        Ok(tomb_hash)
    }

    /// Garbage-collect dead space (spec §8): closed segments whose live
    /// fraction fell below `config.gc_utilization_threshold` are cleaned —
    /// live extents rewritten into fresh space, the segment's bytes
    /// returned to the allocator's free lists.
    ///
    /// Safety protocol: checkpoint first (WAL reset → the checkpointed
    /// refcount view is the only one that matters), clean, update the
    /// engine indices to the new locations, checkpoint again. A crash
    /// mid-GC merely wastes the reclaimed bytes: log-structured stores
    /// never overwrite in place, so pre-GC locations stay readable until
    /// the free lists are actually reused, and the free lists only become
    /// durable with the closing checkpoint.
    pub fn gc(&mut self) -> EngineResult<GcReport> {
        self.checkpoint()?;
        let mut report = GcReport::default();
        loop {
            // Pick the dirtiest cleanable segment (spec §8.1: cost-benefit
            // favors the lowest live fraction). EC-stripe members never
            // move — relocating one member would invalidate the stripe.
            let mut best: Option<crate::store::SegmentInfo> = None;
            for info in self.store.segment_infos() {
                if info.has_ec_extents {
                    continue;
                }
                // Empty husks (rotation artifacts) and healthy segments
                // are not worth a clean pass.
                if info.total_bytes == 0
                    || info.live_fraction >= self.config.gc_utilization_threshold
                {
                    continue;
                }
                if best.is_none_or(|b| info.live_fraction < b.live_fraction) {
                    best = Some(info);
                }
            }
            let Some(info) = best else { break };

            let (cleaned, moves) = self.store.clean_segment(info.seg_idx)?;
            report.segments_cleaned += 1;
            report.extents_moved += cleaned.extents_moved;
            report.bytes_moved += cleaned.bytes_moved;
            report.bytes_reclaimed += cleaned.bytes_reclaimed;

            // Point every index at the new locations. EC membership is
            // tracked per-hash in `chunk_map`/`ec_groups` (shards are
            // referenced by hash, not by location), so preserving the group
            // id through the move keeps self-heal working at the new spot.
            for (hash, new_desc) in moves {
                if let Some(sc) = self.chunk_map.get_mut(&hash) {
                    let mut moved_desc = new_desc;
                    moved_desc.ecc_group_id = sc.desc.ecc_group_id;
                    sc.desc = moved_desc;
                }
                self.dedup.relocate(
                    &self.domain,
                    &hash,
                    crate::dedup::ChunkLocation {
                        zone_id: new_desc.zone_id,
                        offset_in_zone: new_desc.offset_in_zone,
                        length: new_desc.length,
                    },
                );
            }
        }

        // Drop chunk_map entries whose refcount reached zero — no version
        // in the DAG references them anymore (delete releases exactly the
        // live head's refs; time-travel versions keep theirs).
        let dead: Vec<Hash256> = self
            .chunk_map
            .iter()
            .filter(|(h, _)| self.store.refcount(h) == 0)
            .map(|(h, _)| *h)
            .collect();
        for h in dead {
            self.chunk_map.remove(&h);
        }

        if report.segments_cleaned > 0 {
            // Persist the relocations + free lists.
            self.checkpoint()?;
        }
        report.free_bytes_now = self.store.free_bytes();
        Ok(report)
    }

    // -- snapshots / time travel / diff (spec §9) --------------------------------

    pub fn snapshot(&mut self, name: &str) -> Hash256 {
        // Persist the snapshot in the WAL too (crash-safe).
        let root = self.namespace.snapshot(name);
        let _ = self.wal.commit_batch(vec![WalRecord::SnapshotOp {
            name: name.to_string(),
            root,
        }]);
        self.stats.wal_fsyncs += 1;
        root
    }

    pub fn restore(&mut self, name: &str) -> EngineResult<Hash256> {
        let root = self.namespace.restore(name)?;
        // Rebuild the path index from the restored tree.
        self.rebuild_path_index()?;
        let ts = self.namespace.repo.get(&root)?.timestamp_ns;
        let _ = self.wal.commit_batch(vec![WalRecord::HeadOp { root, ts }]);
        self.stats.wal_fsyncs += 1;
        Ok(root)
    }

    fn rebuild_path_index(&mut self) -> EngineResult<()> {
        // Walk the namespace DFS, re-inserting live paths.
        let root = self.namespace.root();
        self.path_index = crate::btree::BeTree::new(crate::btree::TreeConfig {
            max_children: 8,
            buffer_capacity: 32,
            leaf_max_entries: 32,
        });
        let mut stack = vec![(root, String::new())];
        while let Some((dir, prefix)) = stack.pop() {
            let node = self.namespace.repo.get(&dir)?.clone();
            if node.kind != NodeKind::Directory {
                continue;
            }
            for (name, child) in self.namespace.children_public(&dir)? {
                let path = if prefix.is_empty() {
                    format!("/{name}")
                } else {
                    format!("{prefix}/{name}")
                };
                let child_node = self.namespace.repo.get(&child)?.clone();
                if child_node.kind == NodeKind::Directory {
                    stack.push((child, path));
                } else if child_node.kind == NodeKind::File {
                    self.path_index.insert(path.as_bytes(), child.0.to_vec());
                }
            }
        }
        Ok(())
    }

    pub fn diff_snapshots(&self, a: &str, b: &str) -> EngineResult<Vec<DiffEntry>> {
        let ra = self
            .namespace
            .snapshot_root(a)
            .ok_or_else(|| EngineError::NotFound(format!("snapshot {a}")))?;
        let rb = self
            .namespace
            .snapshot_root(b)
            .ok_or_else(|| EngineError::NotFound(format!("snapshot {b}")))?;
        Ok(self.namespace.diff(ra, rb)?)
    }

    /// List live paths (optionally under a prefix).
    pub fn list(&mut self, prefix: Option<&str>) -> Vec<String> {
        self.path_index.flush_all();
        let (lo, hi) = match prefix {
            Some(p) => {
                let lo = p.as_bytes().to_vec();
                let mut hi = lo.clone();
                // Increment last byte for the upper bound.
                if let Some(last) = hi.last_mut() {
                    *last = last.wrapping_add(1);
                }
                (Some(lo), Some(hi))
            }
            None => (None, None),
        };
        self.path_index
            .scan(lo.as_deref(), hi.as_deref())
            .into_iter()
            .map(|(k, _)| String::from_utf8_lossy(&k).to_string())
            .collect()
    }

    // -- scrubbing (spec §11) -----------------------------------------------------

    /// Scrub every extent: verify checksums, heal via EC where possible.
    pub fn scrub(&mut self) -> EngineResult<ScrubReport> {
        self.stats.scrubs += 1;
        let mut report = ScrubReport::default();
        let hashes: Vec<Hash256> = self.chunk_map.keys().copied().collect();
        for h in hashes {
            report.extents_checked += 1;
            let sc = self.chunk_map[&h].clone();
            if self.fetch_and_verify(&sc).is_ok() {
                continue;
            }
            report.corruptions_found += 1;
            if self.ec.is_some() && self.ec_groups.contains_key(&sc.desc.ecc_group_id) {
                match self.heal_chunk(&h) {
                    Ok(_) => report.healed += 1,
                    Err(_) => report.unhealable += 1,
                }
            } else {
                report.unhealable += 1;
            }
        }
        Ok(report)
    }

    /// Test/demo hook: corrupt the stored bytes of a chunk (simulates
    /// silent bit-rot; the scrubber must then heal it).
    pub fn corrupt_chunk(&mut self, hash: &Hash256) -> EngineResult<()> {
        let sc = self
            .chunk_map
            .get(hash)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(format!("chunk {}", hash.short())))?;
        let payload = self.store.read_extent(&sc.desc)?;
        let mut corrupted = payload.clone();
        if corrupted.is_empty() {
            corrupted = vec![1u8, 2, 3];
        } else {
            corrupted[0] ^= 0xFF;
        }
        self.store.write_raw(sc.desc.zone_id, sc.desc.offset_in_zone, &corrupted)?;
        Ok(())
    }

    // -- accessors -----------------------------------------------------------------

    pub fn stats(&self) -> EngineStats {
        self.stats.clone()
    }

    /// On-disk device backing this filesystem, if any (diagnostics /
    /// operators). `None` only for hypothetical in-memory engines.
    pub fn device_path(&self) -> Option<&Path> {
        self.device_path.as_deref()
    }

    pub fn dedup_stats(&self) -> crate::dedup::DedupStats {
        self.dedup.stats()
    }

    pub fn wal_stats(&self) -> crate::wal::WalStats {
        self.wal.stats()
    }

    pub fn root(&self) -> Hash256 {
        self.namespace.root()
    }

    pub fn resolve(&self, path: &str) -> Option<Hash256> {
        self.namespace.resolve(path).ok()
    }

    pub fn version_info(&self, hash: &Hash256) -> Option<crate::core::VersionNode> {
        self.namespace.repo.get(hash).ok().cloned()
    }

    pub fn ec_group_count(&self) -> usize {
        self.ec_groups.len()
    }

    /// Chunk count currently mapped.
    pub fn chunk_count(&self) -> usize {
        self.chunk_map.len()
    }

    /// The chunk hashes composing a file's current version (test/demo API).
    pub fn chunk_hashes_of(&mut self, path: &str) -> EngineResult<Vec<Hash256>> {
        let head = self.namespace.resolve(path)?;
        let node = self.namespace.repo.get(&head)?.clone();
        let meta = self.read_chunk(&node.content_hash)?;
        Ok(bincode::deserialize(&meta)?)
    }

    /// Tier placeholder for the placement engine (spec §12): all prototype
    /// data is on one device; the scorer is exercised via hfs-tier.
    pub fn tier_of(_path: &str) -> Tier {
        Tier::Nvme
    }
}

// Checkpoint serialization ----------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct CheckpointState {
    namespace: crate::dag::NamespaceState,
    dedup: crate::dedup::DedupState,
    chunk_map: HashMap<Hash256, StoredChunk>,
    ec_groups: HashMap<u8, EcGroup>,
    next_ec_group: u8,
    path_index: crate::btree::BeTree<Vec<u8>>,
    vclock: VectorClock,
    hlc: crate::core::Hlc,
    node_id: crate::core::NodeId,
    master_key: [u8; 32],
    domain: Vec<u8>,
    cipher: crate::crypto::Cipher,
    stats: EngineStats,
    /// Allocator state (zone cursors, refcounts, free lists) — persisted
    /// since v1.1; losing it clobbered live extents after remount.
    store_state: crate::store::StoreState,
}

/// Current wall-clock time in ns since the UNIX epoch (0 if the clock is
/// set before the epoch — the engine floors this at [`ClusterEngine::MIN_TS_NS`]).
fn wall_ns_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Compress with a 1-byte tag: `0` = raw, `1` = zstd. Applied *before*
/// encryption (encrypt-then-compress is meaningless).
fn zstd_compress(data: &[u8]) -> Vec<u8> {
    match zstd::encode_all(data, 3) {
        Ok(z) if z.len() + 1 < data.len() => {
            let mut out = vec![1u8];
            out.extend_from_slice(&z);
            out
        }
        _ => {
            let mut out = vec![0u8];
            out.extend_from_slice(data);
            out
        }
    }
}

fn zstd_decompress(data: &[u8]) -> EngineResult<Vec<u8>> {
    match data.first() {
        Some(0) => Ok(data[1..].to_vec()),
        Some(1) => {
            zstd::decode_all(&data[1..]).map_err(|e| EngineError::Corruption(format!("zstd: {e}")))
        }
        _ => Err(EngineError::Corruption("missing compression tag".into())),
    }
}
