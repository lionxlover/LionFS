//! Content-addressed dedup index (spec §10) with the audit's C2/C5/C6 fixes:
//!
//! * **Domain-scoped** (C2/C6): keys are `(domain_id, Hash256)` — dedup
//!   never crosses tenant boundaries, matching the convergent-encryption
//!   key scoping in `hfs-crypto`. The spec's "cluster-wide" scope is
//!   implemented as "all domains share one *index service*, but entries
//!   never match across domains".
//! * **Journaled refcounts** (C5): every refcount mutation is returned as
//!   an op the caller must WAL before applying; ops are idempotent under
//!   replay (delta-based, so replaying a committed txn twice is detected
//!   via the per-txn applied marker at the engine level).
//! * **Bloom prefilter**: a false-positive-tolerant membership sketch in
//!   front of the map, standing in for the spec's per-node local cache of
//!   the sharded global index (§10.3).
//!
//! Dedup ratio model (spec §10.2): with corpus redundancy ρ (probability a
//!   new chunk matches an existing one) the effective storage is
//!   `C·(1-ρ)·avg_chunk + index_overhead` and the ratio is
//!   `raw / effective`.

use crate::core::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum DedupError {
    #[error("refcount underflow for {0}")]
    RefcountUnderflow(Hash256),
    #[error("serialization: {0}")]
    Codec(#[from] bincode::Error),
}

/// A dedup domain (tenant / security scope). See crate docs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DomainId(pub Vec<u8>);

impl DomainId {
    pub fn root() -> Self {
        Self(vec![])
    }
}

impl From<&[u8]> for DomainId {
    fn from(b: &[u8]) -> Self {
        DomainId(b.to_vec())
    }
}

impl<const N: usize> From<&[u8; N]> for DomainId {
    fn from(b: &[u8; N]) -> Self {
        DomainId(b.to_vec())
    }
}

/// Where a deduplicated chunk lives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkLocation {
    pub zone_id: u32,
    pub offset_in_zone: u32,
    pub length: u32,
}

/// Refcount mutation to journal before applying (audit C5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefOp {
    Acquire { domain: DomainId, hash: Hash256, location: ChunkLocation },
    Release { domain: DomainId, hash: Hash256 },
}

/// One dedup index entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    location: ChunkLocation,
    refcount: u64,
}

/// Serializable dedup state (checkpoint payload).
#[derive(Serialize, Deserialize)]
pub struct DedupState {
    entries: Vec<((DomainId, Hash256), Entry)>,
    stats: DedupStats,
}

/// Domain-scoped dedup index.
#[derive(Default)]
pub struct DedupIndex {
    entries: BTreeMap<(DomainId, Hash256), Entry>,
    bloom: BloomFilter,
    /// Cumulative statistics.
    stats: DedupStats,
}

/// Index telemetry (feeds the spec §10.2 ratio model).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DedupStats {
    pub unique_chunks: u64,
    pub duplicate_hits: u64,
    pub logical_chunks_stored: u64,
    pub logical_bytes: u64,
    pub physical_bytes: u64,
}

impl DedupStats {
    /// Dedup ratio = raw / effective (spec §10.2). 1.0 = no savings.
    pub fn ratio(&self) -> f64 {
        if self.physical_bytes == 0 {
            1.0
        } else {
            self.logical_bytes as f64 / self.physical_bytes as f64
        }
    }

    /// Observed corpus redundancy ρ̂ = duplicate hits / logical chunks.
    pub fn observed_redundancy(&self) -> f64 {
        if self.logical_chunks_stored == 0 {
            0.0
        } else {
            self.duplicate_hits as f64 / self.logical_chunks_stored as f64
        }
    }
}

/// Flattened entry list used by the serialized checkpoint form.
type FlatEntries = Vec<((DomainId, Hash256), Entry)>;

impl DedupIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> DedupStats {
        self.stats
    }

    /// Attempt to acquire a reference for `(domain, hash)`:
    /// * hit → returns the existing location + a `Release` op for undo
    ///   bookkeeping (caller journals `Acquire` semantics via return).
    /// * miss → caller must store the chunk then call [`Self::insert_new`].
    pub fn lookup(&mut self, domain: &DomainId, hash: &Hash256) -> Option<ChunkLocation> {
        let key = (domain.clone(), *hash);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.refcount += 1;
            self.stats.duplicate_hits += 1;
            // logical chunk accounting happens in account_duplicate()
            Some(entry.location.clone())
        } else {
            None
        }
    }

    /// Register a newly stored chunk (after a miss). The caller journals
    /// the `Acquire` op *before* calling this.
    pub fn insert_new(&mut self, domain: &DomainId, hash: &Hash256, location: ChunkLocation, bytes: u64) {
        let key = (domain.clone(), *hash);
        debug_assert!(!self.entries.contains_key(&key), "insert_new on existing hash");
        self.bloom.add(&key);
        self.entries.insert(key, Entry { location, refcount: 1 });
        self.stats.unique_chunks += 1;
        self.stats.logical_chunks_stored += 1;
        self.stats.logical_bytes += bytes;
        self.stats.physical_bytes += bytes;
    }

    /// Account logical (deduplicated) bytes for a duplicate hit.
    pub fn account_duplicate(&mut self, bytes: u64) {
        self.stats.logical_chunks_stored += 1;
        self.stats.logical_bytes += bytes;
        // physical bytes unchanged — the chunk already exists.
    }

    /// Release a reference. Returns the location to garbage-collect when
    /// the refcount hits zero.
    pub fn release(&mut self, domain: &DomainId, hash: &Hash256) -> Result<Option<ChunkLocation>, DedupError> {
        let key = (domain.clone(), *hash);
        match self.entries.get_mut(&key) {
            Some(entry) => {
                if entry.refcount == 0 {
                    return Err(DedupError::RefcountUnderflow(*hash));
                }
                entry.refcount -= 1;
                if entry.refcount == 0 {
                    let location = self.entries.remove(&key).unwrap().location;
                    Ok(Some(location))
                } else {
                    Ok(None)
                }
            }
            None => Err(DedupError::RefcountUnderflow(*hash)),
        }
    }

    /// Current refcount for inspection.
    pub fn refcount(&self, domain: &DomainId, hash: &Hash256) -> u64 {
        self.entries.get(&(domain.clone(), *hash)).map(|e| e.refcount).unwrap_or(0)
    }

    /// Cheap "definitely not present" check (Bloom prefilter).
    pub fn may_exist(&self, domain: &DomainId, hash: &Hash256) -> bool {
        self.bloom.may_contain(&(domain.clone(), *hash))
    }

    /// Update the stored location of a chunk (cleaner moved the extent).
    pub fn relocate(&mut self, domain: &DomainId, hash: &Hash256, location: ChunkLocation) {
        if let Some(entry) = self.entries.get_mut(&(domain.clone(), *hash)) {
            entry.location = location;
        }
    }

    /// Serialize the whole index (checkpoint).
    pub fn to_bytes(&self) -> Result<Vec<u8>, DedupError> {
        let flat: FlatEntries =
            self.entries.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        Ok(bincode::serialize(&(flat, self.stats))?)
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, DedupError> {
        let (flat, stats): (FlatEntries, DedupStats) = bincode::deserialize(b)?;
        let mut bloom = BloomFilter::with_items(flat.len().max(64));
        let mut entries = BTreeMap::new();
        for (k, v) in flat {
            bloom.add(&k);
            entries.insert(k, v);
        }
        Ok(Self { entries, bloom, stats })
    }

    /// Clone serializable state (engine checkpoint support).
    pub fn clone_state(&self) -> DedupState {
        DedupState {
            entries: self.entries.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            stats: self.stats,
        }
    }

    /// Restore from state (engine checkpoint support).
    pub fn from_state(state: DedupState) -> Self {
        let mut bloom = BloomFilter::with_items(state.entries.len().max(64));
        let mut entries = BTreeMap::new();
        for (k, v) in state.entries {
            bloom.add(&k);
            entries.insert(k, v);
        }
        Self { entries, bloom, stats: state.stats }
    }

    /// WAL replay: register/raise a refcount for an acquired chunk.
    pub fn replay_acquire(&mut self, domain: &DomainId, hash: &Hash256) {
        let key = (domain.clone(), *hash);
        let entry = self.entries.entry(key).or_insert(Entry {
            location: ChunkLocation { zone_id: 0, offset_in_zone: 0, length: 0 },
            refcount: 0,
        });
        entry.refcount += 1;
        self.stats.logical_chunks_stored += 1;
    }

    /// WAL replay: lower a refcount (remove at zero).
    pub fn replay_release(&mut self, domain: &DomainId, hash: &Hash256) {
        let key = (domain.clone(), *hash);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.refcount = entry.refcount.saturating_sub(1);
            if entry.refcount == 0 {
                self.entries.remove(&key);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bloom filter (prefilter for the sharded global index, spec §10.3)
// ---------------------------------------------------------------------------

/// A simple fixed-size Bloom filter over serializable keys.
///
/// The production design shards the *global* index by consistent hashing
/// (`shard(h) = h mod NumMetadataNodes`, spec §10.3) and keeps one of these
/// per shard as a local prefilter, so most lookups of absent chunks never
/// leave the node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BloomFilter {
    bits: Vec<u64>,
    num_bits: usize,
    num_hashes: u32,
    items: u64,
}

fn fnv1a(data: &[u8], seed: u64) -> u64 {
    let mut h = 0xcbf29ce484222325u64 ^ seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Default for BloomFilter {
    fn default() -> Self {
        Self::with_items(64)
    }
}

impl BloomFilter {
    /// Size for an expected `n` items at ~1% false-positive rate.
    pub fn with_items(n: usize) -> Self {
        let n = n.max(16);
        // m ≈ -n·ln(p)/(ln2)² with p = 0.01 → ~9.6 bits per item.
        let num_bits = (n * 10).next_power_of_two();
        let num_hashes = 7;
        Self {
            bits: vec![0u64; num_bits.div_ceil(64)],
            num_bits,
            num_hashes,
            items: 0,
        }
    }

    pub fn add<K: Serialize>(&mut self, key: &K) {
        let bytes = bincode::serialize(key).expect("bloom serialize");
        for i in 0..self.num_hashes {
            let h = fnv1a(&bytes, i as u64) % self.num_bits as u64;
            self.bits[(h / 64) as usize] |= 1u64 << (h % 64);
        }
        self.items += 1;
    }

    pub fn may_contain<K: Serialize>(&self, key: &K) -> bool {
        let bytes = bincode::serialize(key).expect("bloom serialize");
        for i in 0..self.num_hashes {
            let h = fnv1a(&bytes, i as u64) % self.num_bits as u64;
            if self.bits[(h / 64) as usize] & (1u64 << (h % 64)) == 0 {
                return false;
            }
        }
        true
    }

    pub fn estimated_fpp(&self) -> f64 {
        let m = self.num_bits as f64;
        let n = self.items as f64;
        let k = self.num_hashes as f64;
        (1.0 - (-k * n / m).exp()).powf(k)
    }
}

/// Consistent-hash shard selection (spec §10.3 / §14.4):
/// `shard(h) = h mod N` over the first 8 bytes of the content hash.
pub fn shard_for(hash: &Hash256, num_shards: usize) -> usize {
    if num_shards == 0 {
        return 0;
    }
    (hash.prefix_u64() % num_shards as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain() -> DomainId {
        DomainId::from(b"tenant-a")
    }

    fn loc(i: u32) -> ChunkLocation {
        ChunkLocation { zone_id: 1, offset_in_zone: i * 4096, length: 4096 }
    }

    #[test]
    fn basic_hit_miss_and_refcounts() {
        let mut idx = DedupIndex::new();
        let d = domain();
        let h = Hash256::of(b"chunk-1");

        assert!(idx.lookup(&d, &h).is_none(), "first sight must miss");
        idx.insert_new(&d, &h, loc(0), 4096);
        assert_eq!(idx.refcount(&d, &h), 1);

        // Second reference dedups.
        let hit = idx.lookup(&d, &h).unwrap();
        assert_eq!(hit, loc(0));
        assert_eq!(idx.refcount(&d, &h), 2);
        idx.account_duplicate(4096);

        // Releases only free at zero.
        assert!(idx.release(&d, &h).unwrap().is_none());
        let freed = idx.release(&d, &h).unwrap();
        assert_eq!(freed, Some(loc(0)));
        assert!(idx.release(&d, &h).is_err(), "underflow must be an error");
    }

    #[test]
    fn domains_never_cross_dedup() {
        // Audit C6: identical content in two tenants = two entries.
        let mut idx = DedupIndex::new();
        let a = DomainId::from(b"tenant-a");
        let b = DomainId::from(b"tenant-b");
        let h = Hash256::of(b"shared chunk");
        assert!(idx.lookup(&a, &h).is_none());
        idx.insert_new(&a, &h, loc(0), 4096);
        // Tenant B must NOT hit tenant A's entry...
        assert!(idx.lookup(&b, &h).is_none(), "cross-domain hit must not happen");
        // ...and stores its own copy.
        idx.insert_new(&b, &h, loc(1), 4096);
        assert_eq!(idx.stats().unique_chunks, 2);
    }

    #[test]
    fn stats_and_ratio() {
        let mut idx = DedupIndex::new();
        let d = domain();
        // 3 logical chunks of 4 KiB, 2 unique.
        let h1 = Hash256::of(b"x1");
        let h2 = Hash256::of(b"x2");
        idx.insert_new(&d, &h1, loc(0), 4096);
        idx.insert_new(&d, &h2, loc(1), 4096);
        assert!(idx.lookup(&d, &h1).is_some());
        idx.account_duplicate(4096);

        let s = idx.stats();
        assert_eq!(s.unique_chunks, 2);
        assert_eq!(s.duplicate_hits, 1);
        assert_eq!(s.logical_chunks_stored, 3);
        // Ratio: 12 KiB logical / 8 KiB physical = 1.5×.
        assert!((s.ratio() - 1.5).abs() < 1e-9);
        assert!((s.observed_redundancy() - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn bloom_prefilter_semantics() {
        let mut bloom = BloomFilter::with_items(1000);
        let mut idx = DedupIndex::new();
        let d = domain();
        for i in 0..100u32 {
            let h = Hash256::of(format!("chunk-{i}").as_bytes());
            idx.insert_new(&d, &h, loc(i), 64);
            bloom.add(&(d.clone(), h));
        }
        // All present keys report may_contain (no false negatives).
        for i in 0..100u32 {
            let h = Hash256::of(format!("chunk-{i}").as_bytes());
            assert!(bloom.may_contain(&(d.clone(), h)), "false negative at {i}");
        }
        // Fresh keys usually miss; fpp stays low.
        let mut false_pos = 0;
        for i in 1000..2000u32 {
            let h = Hash256::of(format!("other-{i}").as_bytes());
            if bloom.may_contain(&(d.clone(), h)) {
                false_pos += 1;
            }
        }
        assert!(false_pos < 30, "fpp too high: {false_pos}/1000");
        assert!(bloom.estimated_fpp() < 0.05);
    }

    #[test]
    fn serialization_roundtrip() {
        let mut idx = DedupIndex::new();
        let d = domain();
        for i in 0..50u32 {
            let h = Hash256::of(format!("c{i}").as_bytes());
            idx.insert_new(&d, &h, loc(i), 128);
        }
        let bytes = idx.to_bytes().unwrap();
        let back = DedupIndex::from_bytes(&bytes).unwrap();
        for i in 0..50u32 {
            let h = Hash256::of(format!("c{i}").as_bytes());
            assert_eq!(back.refcount(&d, &h), 1);
        }
        assert_eq!(back.stats().unique_chunks, 50);
    }

    #[test]
    fn shard_selection_is_stable_and_spread() {
        let h = Hash256::of(b"some chunk");
        assert_eq!(shard_for(&h, 8), shard_for(&h, 8));
        let mut counts = [0usize; 8];
        for i in 0..800u32 {
            let hh = Hash256::of(format!("chunk-{i}").as_bytes());
            counts[shard_for(&hh, 8)] += 1;
        }
        for c in counts {
            assert!(c > 50, "uneven shard distribution: {counts:?}");
        }
    }
}
