//! The three-level dedup index (RFC-002 §7.2).
//!
//! Duplicate hits append a reference to the existing chunk extent under
//! the refcount tree; misses write the chunk once. The memory budget is
//! explicit and bounded: the bloom filter and hot cache together default
//! to 0.1% of pool size in RAM (1 GB per TB). The honest consequence --
//! a cold duplicate costs one hash-tree walk -- is priced in the RFC's
//! Table 19 rather than hidden. Inline dedup is disabled on hot pools by
//! default because deduplication randomizes layout.
//!
//! Levels:
//! 1. **Bloom filter** over the whole pool: definitely-absent answers
//!    are free; a "maybe" falls through.
//! 2. **Hot LRU** of recently-seen chunk hashes: the common duplicate
//!    (same file re-written, adjacent backups) hits in RAM.
//! 3. **On-disk hash tree** (the 1.x format's dedup tree): consulted
//!    only when both filters say maybe. The caller supplies the lookup
//!    closure so this module stays disk-free and testable.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A 128-bit chunk hash (BLAKE3-128 truncated).
pub type ChunkHash = [u8; 16];
/// Default RAM budget: 0.1% of pool size.
pub const RAM_BUDGET_FRAC_PER_MILLI: u64 = 1; // per 1000 => 0.1%

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupProbe {
    /// Definitely not in the pool: write the chunk.
    Miss,
    /// In the hot cache: reference the known extent.
    HotHit { physical: u64, len: u32 },
    /// Maybe on disk: consult the hash tree.
    ColdCandidate,
}

/// Fixed-size bloom filter, seeded from the chunk hash itself (no
/// stored state beyond the bits: k=4 probes derived by re-mixing).
#[derive(Debug)]
pub struct BloomIndex {
    bits: Vec<AtomicU64>,
    mask: u64,
    /// Inserted distinct items (approximate, for sizing math).
    inserted: AtomicU64,
}

impl BloomIndex {
    /// Sizes the filter for `expected_items` at ~1% false-positive rate
    /// (the classic ~9.6 bits/item bound, rounded to words).
    #[must_use]
    pub fn with_capacity(expected_items: usize) -> Self {
        let items = expected_items.max(64);
        let bits = items * 10; // ~1% FP at k=4
        let words = bits.div_ceil(64).next_power_of_two();
        Self {
            bits: (0..words).map(|_| AtomicU64::new(0)).collect(),
            mask: (words - 1) as u64,
            inserted: AtomicU64::new(0),
        }
    }

    /// RAM footprint in bytes (the budget the RFC bounds).
    pub fn ram_bytes(&self) -> usize {
        self.bits.len() * 8
    }

    fn positions(hash: &ChunkHash) -> [u64; 4] {
        // Derive 4 independent 64-bit probes by re-mixing the 128-bit
        // hash (splitmix over both halves, then chained).
        let lo = u64::from_le_bytes(hash[0..8].try_into().expect("8 bytes"));
        let hi = u64::from_le_bytes(hash[8..16].try_into().expect("8 bytes"));
        let m = crate::io_engine::shard::splitmix64(lo ^ hi);
        let mut probes = [0u64; 4];
        let mut s = m;
        for p in probes.iter_mut() {
            s = crate::io_engine::shard::splitmix64(s);
            *p = s;
        }
        probes
    }

    /// Inserts a chunk hash.
    pub fn insert(&self, hash: &ChunkHash) {
        for probe in Self::positions(hash) {
            let idx = (probe & self.mask) as usize;
            let word = (probe >> 32) as u64 % 64;
            // SAFETY of concurrency: multiple insertors may race a word,
            // but set-bit races are idempotent (OR semantics).
            self.bits[idx].fetch_or(1u64 << word, Ordering::Relaxed);
        }
        self.inserted.fetch_add(1, Ordering::Relaxed);
    }

    /// `false` = definitely absent; `true` = maybe present.
    #[must_use]
    pub fn may_contain(&self, hash: &ChunkHash) -> bool {
        Self::positions(hash).iter().all(|probe| {
            let idx = (*probe & self.mask) as usize;
            let word = (*probe >> 32) as u64 % 64;
            self.bits[idx].load(Ordering::Relaxed) & (1u64 << word) != 0
        })
    }

    pub fn inserted_items(&self) -> u64 {
        self.inserted.load(Ordering::Relaxed)
    }
}

/// Bounded LRU of hot chunk hashes -> physical extent.
struct HotLru {
    map: HashMap<ChunkHash, (u64, u32)>,
    order: std::collections::VecDeque<ChunkHash>,
    capacity: usize,
    hits: u64,
    misses: u64,
}

impl HotLru {
    fn get(&mut self, hash: &ChunkHash) -> Option<(u64, u32)> {
        match self.map.get(hash) {
            Some(v) => {
                self.hits += 1;
                // Refresh LRU position.
                if let Some(pos) = self.order.iter().position(|h| h == hash) {
                    self.order.remove(pos);
                    self.order.push_back(*hash);
                }
                Some(*v)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    fn put(&mut self, hash: ChunkHash, physical: u64, len: u32) {
        if self.map.len() >= self.capacity && !self.map.contains_key(&hash) {
            if let Some(evict) = self.order.pop_front() {
                self.map.remove(&evict);
            }
        }
        self.order.push_back(hash);
        self.map.insert(hash, (physical, len));
    }
}

/// The three-level index.
pub struct DedupIndex {
    bloom: BloomIndex,
    hot: Mutex<HotLru>,
    /// Bloom "maybe" count (falls to the hash-tree walk: the cost the
    /// RFC prices).
    maybe_probes: AtomicU64,
    hot_hits: AtomicU64,
    misses: AtomicU64,
}

impl DedupIndex {
    /// Builds the index sized for `pool_bytes`: bloom for the expected
    /// distinct chunk count (pool / avg chunk) and a hot LRU within the
    /// 0.1% RAM budget.
    #[must_use]
    pub fn sized_for_pool(pool_bytes: u64) -> Self {
        // The RFC's explicit, bounded budget: bloom + hot cache together
        // default to 0.1% of pool size in RAM (1 GB per TB).
        let budget = (pool_bytes / 1000) as usize; // bytes
        let expected_chunks =
            (pool_bytes / crate::pipeline::fastcdc::CHUNK_AVG as u64).max(1024) as usize;
        // Split 2:1 bloom:hot. Bloom: ~10 bits/item, so its item capacity
        // is whatever fits in 2/3 of the budget.
        let bloom_bytes = (budget / 3) * 2;
        let bloom_by_budget = bloom_bytes * 8 / 10; // bits of budget -> items
        let bloom_items = expected_chunks.min(bloom_by_budget);
        // Hot LRU: 16-byte key + 8+4 value + node overhead ~ 24-40 B/entry.
        let hot_entries = (budget / 3 / 24).min(1 << 20);
        Self::new(bloom_items.max(1024), hot_entries.max(1024))
    }

    #[must_use]
    pub fn new(bloom_items: usize, hot_entries: usize) -> Self {
        Self {
            bloom: BloomIndex::with_capacity(bloom_items),
            hot: Mutex::new(HotLru {
                map: HashMap::with_capacity(hot_entries.min(4096)),
                order: std::collections::VecDeque::with_capacity(hot_entries.min(4096)),
                capacity: hot_entries,
                hits: 0,
                misses: 0,
            }),
            maybe_probes: AtomicU64::new(0),
            hot_hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// RAM budget consumed (the observable the RFC bounds to 0.1%).
    pub fn ram_bytes(&self) -> usize {
        let hot = self
            .hot
            .lock()
            .map(|h| h.capacity * 24 + h.order.capacity() * 16)
            .unwrap_or(0);
        self.bloom.ram_bytes() + hot
    }

    /// Probes the index for a chunk hash.
    #[must_use]
    pub fn probe(&self, hash: &ChunkHash) -> DedupProbe {
        if let Some(hit) = self.hot.lock().expect("hot lru lock").get(hash) {
            self.hot_hits.fetch_add(1, Ordering::Relaxed);
            return DedupProbe::HotHit {
                physical: hit.0,
                len: hit.1,
            };
        }
        if self.bloom.may_contain(hash) {
            self.maybe_probes.fetch_add(1, Ordering::Relaxed);
            DedupProbe::ColdCandidate
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            DedupProbe::Miss
        }
    }

    /// Records a chunk that was written (miss path).
    pub fn record_new(&self, hash: &ChunkHash, physical: u64, len: u32) {
        self.bloom.insert(hash);
        self.hot
            .lock()
            .expect("hot lru lock")
            .put(*hash, physical, len);
    }

    /// Promotes a cold duplicate found in the hash tree into the hot
    /// cache (one walk, then RAM-served).
    pub fn promote_cold_hit(&self, hash: &ChunkHash, physical: u64, len: u32) {
        self.hot
            .lock()
            .expect("hot lru lock")
            .put(*hash, physical, len);
    }

    /// Health-bus stats: (hot hits, bloom maybes, misses).
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.hot_hits.load(Ordering::Relaxed),
            self.maybe_probes.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }
}

/// Computes the BLAKE3-128 chunk hash (the RFC's choice).
#[must_use]
pub fn chunk_hash(data: &[u8]) -> ChunkHash {
    let digest = blake3::hash(data); // 32 bytes
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest.as_bytes()[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_definitely_absent_for_unseen_hashes() {
        let b = BloomIndex::with_capacity(10_000);
        let h1 = chunk_hash(b"alpha");
        let h2 = chunk_hash(b"beta");
        assert!(!b.may_contain(&h1));
        b.insert(&h1);
        assert!(b.may_contain(&h1));
        // Unseen: with 10k capacity and 1 item, a false positive is
        // ~1e-100; this is a deterministic test of emptiness, not luck.
        assert!(!b.may_contain(&h2));
    }

    #[test]
    fn bloom_fp_rate_stays_near_one_percent() {
        // 1% design point: insert n items, probe n fresh ones, count
        // maybes. With n = 20k this is stable.
        let n = 20_000usize;
        let b = BloomIndex::with_capacity(n);
        let mut inserted = Vec::with_capacity(n);
        for i in 0..n as u64 {
            let h = chunk_hash(&i.to_le_bytes());
            b.insert(&h);
            inserted.push(h);
        }
        let mut fps = 0usize;
        for i in 0..n as u64 {
            let h = chunk_hash(&(i + 1_000_000).to_le_bytes());
            if b.may_contain(&h) {
                fps += 1;
            }
        }
        let rate = fps as f64 / n as f64;
        assert!(rate < 0.05, "false positive rate {rate} above design point");
    }

    #[test]
    fn all_inserted_items_are_maybe_present() {
        let n = 5_000usize;
        let b = BloomIndex::with_capacity(n);
        let hashes: Vec<ChunkHash> = (0..n as u64)
            .map(|i| chunk_hash(&i.to_le_bytes()))
            .collect();
        for h in &hashes {
            b.insert(h);
        }
        for h in &hashes {
            assert!(
                b.may_contain(h),
                "inserted item lost (bloom must have no false negatives)"
            );
        }
    }

    #[test]
    fn index_levels_route_correctly() {
        let idx = DedupIndex::new(10_000, 128);
        let h1 = chunk_hash(b"chunk-1");
        // Unseen: miss.
        assert!(matches!(idx.probe(&h1), DedupProbe::Miss));
        idx.record_new(&h1, 0x1000, 8192);
        // Hot now.
        assert_eq!(
            idx.probe(&h1),
            DedupProbe::HotHit {
                physical: 0x1000,
                len: 8192
            }
        );
        let (hot, _maybe, miss) = idx.stats();
        assert_eq!(hot, 1);
        assert_eq!(miss, 1);
    }

    #[test]
    fn cold_candidate_is_promotable() {
        let idx = DedupIndex::new(10_000, 128);
        let h = chunk_hash(b"cold-chunk");
        idx.bloom.insert(&h); // on disk, not hot
        assert_eq!(idx.probe(&h), DedupProbe::ColdCandidate);
        idx.promote_cold_hit(&h, 0x2000, 4096);
        assert_eq!(
            idx.probe(&h),
            DedupProbe::HotHit {
                physical: 0x2000,
                len: 4096
            }
        );
    }

    #[test]
    fn hot_lru_evicts_at_capacity() {
        let idx = DedupIndex::new(10_000, 8);
        for i in 0..16u64 {
            let h = chunk_hash(&i.to_le_bytes());
            idx.record_new(&h, i * 4096, 4096);
        }
        // The earliest entries were evicted; the latest is hot.
        let h_late = chunk_hash(&15u64.to_le_bytes());
        assert!(matches!(idx.probe(&h_late), DedupProbe::HotHit { .. }));
        let h_early = chunk_hash(&0u64.to_le_bytes());
        // Evicted from hot; bloom still says maybe.
        assert_eq!(idx.probe(&h_early), DedupProbe::ColdCandidate);
    }

    #[test]
    fn pool_sizing_keeps_budget_shape() {
        // A 16 GiB pool: the 0.1% budget is ~16.8 MB; the construction
        // must stay within it (CI-friendly size).
        let idx = DedupIndex::sized_for_pool(16 * 1024 * 1024 * 1024);
        let ram = idx.ram_bytes();
        let budget = (16u64 * 1024 * 1024 * 1024 / 1000) as usize;
        assert!(ram <= budget, "ram {ram} exceeded the 0.1% budget {budget}");
        assert!(ram > 0);
    }

    #[test]
    fn chunk_hash_is_stable_and_distinct() {
        assert_eq!(chunk_hash(b"same"), chunk_hash(b"same"));
        assert_ne!(chunk_hash(b"a"), chunk_hash(b"b"));
        assert_eq!(chunk_hash(b"").len(), 16);
    }
}
