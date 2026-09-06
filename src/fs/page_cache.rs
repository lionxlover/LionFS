//! Phase 10 (parallel write path): the write-back intake cache.
//!
//! This is the layer that lets N threads call `VfsOps::write`
//! concurrently on one mounted filesystem. Every OS-grade filesystem
//! (the Linux page cache, ZFS's DMU, the NTFS cache manager) delivers
//! parallel *buffered* writes the same way: the syscall path only
//! copies bytes and returns, while a single staging path (journal /
//! txg / log) batches the real metadata-and-data work. LionFS 3.3 was
//! single-writer end to end: every `write()` walked checksums, the
//! extent map, the allocator, and the journal under one giant
//! `&mut self`. 3.4 splits intake from staging:
//!
//! * **Intake** (this module): per-inode write gates + a plaintext
//!   4-KiB page map. A write copies caller bytes into pages, updates
//!   the inode's shadow size/mtime, and returns. Different inodes
//!   never contend; the same inode serializes on its gate (the same
//!   guarantee POSIX gives two writers of one file).
//! * **Staging** (`vfs_impl::flush_ino`): pages are drained in
//!   contiguous runs through the *unchanged* 3.3 write machinery
//!   (`FileManager::write_file`) under the single transaction/staging
//!   lock -- so every B-tree, allocator, CoW, dedup, and journal
//!   invariant that 736 tests + the crash simulator established for a
//!   single writer still holds by construction. Batched flushes mean
//!   the staging lock is taken rarely and the speculative-run
//!   allocator sees large sequential extents, not 4-KiB dribbles.
//!
//! Durability semantics are the standard write-back contract and match
//! 3.3 exactly where it matters: bytes survive a crash only after the
//! staging transaction that carries them has committed (fsync / flush /
//! the 1024-block commit threshold). In 3.3, uncommitted-but-staged
//! bytes lived in `active_tx`; in 3.4 they live here. Either way they
//! were readable before the crash and are gone after it.
//!
//! Scope decisions, deliberately narrow:
//! * **Compressed inodes bypass the cache** (cluster-granularity path
//!   is stateful and rare; they keep 3.3 semantics under the staging
//!   lock). Encrypted inodes do NOT bypass it: pages hold plaintext,
//!   and flush re-encodes through the normal cipher path.
//! * **No background flusher thread.** Flushing is triggered by
//!   fsync/flush/setattr-size/unlink-drop/destroy and by a dirty-byte
//!   threshold (bounded memory). This keeps every code path
//!   deterministic -- the same property that made the 3.3 crash
//!   simulator's bit-identical replays possible.

use crate::ondisk::serialization::BLOCK_SIZE;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Total dirty bytes across all inodes that triggers a flush of the
/// largest dirty inode. Bounds resident memory; the number is a
/// policy knob, not a correctness bound (correctness never depends on
/// when a flush happens, only that fsync/destroy flush).
pub const FLUSH_THRESHOLD_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct CachedInode {
    /// Logical-block -> plaintext page bytes. BTreeMap so contiguous
    /// runs (the flush unit, and the speculative-append pattern the
    /// allocator likes) fall out of iteration order for free.
    pub pages: BTreeMap<u64, Box<[u8; BLOCK_SIZE]>>,
    /// Observed file size after the newest cached write. The read and
    /// getattr paths take `max(committed_size, shadow_size)` so
    /// read-your-own-buffered-write works exactly like a kernel page
    /// cache.
    pub shadow_size: u64,
    pub shadow_mtime: u64,
    pub dirty_bytes: usize,
    pub last_used: Instant,
}

impl CachedInode {
    fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
            shadow_size: 0,
            shadow_mtime: 0,
            dirty_bytes: 0,
            last_used: Instant::now(),
        }
    }
}

/// One page cache shared by the whole mounted image. `inner` is the
/// only lock on the read path (short critical sections: page copies,
/// never disk I/O), which is what makes concurrent reads + concurrent
/// intake scale.
pub struct PageCache {
    inner: Mutex<Inner>,
    /// Env gate: `LFS_WRITEBACK=0` restores 3.3's write-through
    /// behavior (every write stages synchronously). Keeps a
    /// conservative escape hatch and gives the benchmark suite an A/B
    /// lever. Default ON.
    enabled: bool,
}

struct Inner {
    inodes: HashMap<u64, CachedInode>,
    gates: HashMap<u64, Arc<Mutex<()>>>,
    total_dirty: usize,
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PageCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                inodes: HashMap::new(),
                gates: HashMap::new(),
                total_dirty: 0,
            }),
            enabled: std::env::var("LFS_WRITEBACK")
                .map(|v| v != "0")
                .unwrap_or(true),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The per-inode write gate. Writers (intake and flush) hold this
    /// while touching an inode's pages so two writes to one file can
    /// never interleave a partial page. Cheap to create; entries live
    /// until unlink/evict.
    pub fn gate(&self, ino: u64) -> Arc<Mutex<()>> {
        let mut inner = self.inner.lock().unwrap();
        Arc::clone(inner.gates.entry(ino).or_insert_with(|| Arc::new(Mutex::new(()))))
    }

    /// True if this inode has at least one cached page.
    pub fn has_pages(&self, ino: u64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .inodes
            .get(&ino)
            .is_some_and(|c| !c.pages.is_empty())
    }

    /// True if one specific logical page is cached (the RMW pre-fetch
    /// check on the intake path).
    pub fn has_page(&self, ino: u64, block: u64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .inodes
            .get(&ino)
            .is_some_and(|c| c.pages.contains_key(&block))
    }

    /// Copy out up to `blocks` (a sorted list of logical block numbers).
    /// Returns pages found. Never blocks on I/O; misses are the
    /// caller's problem.
    pub fn get_pages(&self, ino: u64, blocks: &[u64]) -> Vec<(u64, [u8; BLOCK_SIZE])> {
        let inner = self.inner.lock().unwrap();
        let Some(c) = inner.inodes.get(&ino) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(blocks.len());
        for &b in blocks {
            if let Some(p) = c.pages.get(&b) {
                out.push((b, **p));
            }
        }
        out
    }

    /// Effective (shadow) size + mtime for an inode, or `None` when
    /// nothing is cached.
    pub fn shadow(&self, ino: u64) -> Option<(u64, u64)> {
        let inner = self.inner.lock().unwrap();
        inner.inodes.get(&ino).map(|c| (c.shadow_size, c.shadow_mtime))
    }

    /// Store a run of caller bytes for `ino` at `offset` as plaintext
    /// pages, creating or merging pages as needed. The caller passes
    /// `fetch` to fill any page it touches but that is not already
    /// cached: `fetch(logical_block)` must return that block's current
    /// committed/staged plaintext bytes (or zeros beyond EOF). This is
    /// the page-level read-modify-write that makes partial-block
    /// writes correct -- including over encrypted content, because
    /// pages are plaintext by construction.
    ///
    /// MUST be called while holding this inode's gate.
    pub fn store<Fetch>(
        &self,
        ino: u64,
        offset: u64,
        data: &[u8],
        mtime: u64,
        mut fetch: Fetch,
    ) where
        Fetch: FnMut(u64) -> [u8; BLOCK_SIZE],
    {
        if data.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let end = offset + data.len() as u64;
        let c = inner.inodes.entry(ino).or_insert_with(CachedInode::new);
        c.last_used = Instant::now();
        c.shadow_size = c.shadow_size.max(end);
        c.shadow_mtime = mtime;
        let mut pos = 0usize;
        while pos < data.len() {
            let file_off = offset + pos as u64;
            let block = file_off / BLOCK_SIZE as u64;
            let within = (file_off % BLOCK_SIZE as u64) as usize;
            let chunk = (BLOCK_SIZE - within).min(data.len() - pos);
            let existing: Option<[u8; BLOCK_SIZE]> = if within == 0 && chunk == BLOCK_SIZE {
                // Full fresh page: no fetch needed.
                None
            } else {
                // RMW: pull the page we already have, else the caller's
                // fetch (which the vfs layer pre-populates with the
                // committed block content before calling us).
                Some(c.pages.get(&block).map_or_else(|| fetch(block), |p| **p))
            };
            let mut page = existing.unwrap_or([0u8; BLOCK_SIZE]);
            page[within..within + chunk].copy_from_slice(&data[pos..pos + chunk]);
            // Dirty accounting counts a page once while it is resident,
            // not once per overwrite (an 8-MiB rotating window must not
            // look like unbounded new dirty data).
            let newly_dirty = !c.pages.contains_key(&block);
            c.pages.insert(block, Box::new(page));
            if newly_dirty {
                c.dirty_bytes += BLOCK_SIZE;
            }
            pos += chunk;
        }
        inner.total_dirty = inner
            .total_dirty
            .max(inner.inodes.values().map(|c| c.dirty_bytes).sum());
    }

    /// Drain all cached pages for `ino` as contiguous runs, clearing
    /// the cache entry (the shadow dies with the pages: after a flush
    /// the committed inode is the truth again).
    ///
    /// MUST be called while holding this inode's gate.
    pub fn drain_runs(&self, ino: u64) -> Vec<(u64, Vec<u8>)> {
        self.drain_runs_and_eof(ino).0
    }

    /// 3.6 (Phase 12 fix): drain WITH the logical EOF. Storage writes
    /// stay block-granular (the tail page's full 4 KiB lands on the
    /// block -- the intake RMW already merged the committed bytes),
    /// but the caller must NOT let `write_file` extend the file's
    /// size to the page boundary: the logical size is the shadow.
    /// Returns `(runs, shadow_eof)` -- `None` when the inode had no
    /// cached state.
    pub fn drain_runs_and_eof(&self, ino: u64) -> (Vec<(u64, Vec<u8>)>, Option<u64>) {
        let mut inner = self.inner.lock().unwrap();
        let dirty = inner.inodes.get(&ino).map_or(0, |c| c.dirty_bytes);
        inner.total_dirty = inner.total_dirty.saturating_sub(dirty);
        let Some(c) = inner.inodes.get_mut(&ino) else {
            return (Vec::new(), None);
        };
        let pages = std::mem::take(&mut c.pages);
        let shadow_size = c.shadow_size;
        c.dirty_bytes = 0;
        c.shadow_size = 0;
        c.shadow_mtime = 0;
        inner.inodes.remove(&ino);
        (runs_from_pages(pages), if shadow_size > 0 { Some(shadow_size) } else { None })
    }

    /// Drop everything cached for `ino` WITHOUT flushing (unlink of an
    /// unsynced file: the data was never promised durability).
    pub fn drop_ino(&self, ino: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(c) = inner.inodes.remove(&ino) {
            inner.total_dirty = inner.total_dirty.saturating_sub(c.dirty_bytes);
        }
        inner.gates.remove(&ino);
    }

    /// All inodes with cached pages (for destroy-time flush-all).
    pub fn dirty_inodes(&self) -> Vec<u64> {
        self.inner
            .lock()
            .unwrap()
            .inodes
            .iter()
            .filter(|(_, c)| !c.pages.is_empty())
            .map(|(&ino, _)| ino)
            .collect()
    }

    /// The inode holding the most dirty bytes, for threshold flush.
    pub fn largest_dirty(&self) -> Option<u64> {
        let inner = self.inner.lock().unwrap();
        inner
            .inodes
            .iter()
            .filter(|(_, c)| !c.pages.is_empty())
            .max_by_key(|(_, c)| c.dirty_bytes)
            .map(|(&ino, _)| ino)
    }

    pub fn total_dirty(&self) -> usize {
        self.inner.lock().unwrap().total_dirty
    }
}

/// Turn a page map into contiguous `(start_logical_block, bytes)`
/// runs. Contiguous runs are what the flush path feeds to
/// `FileManager::write_file` so the speculative-run allocator gets
/// whole extents per flush instead of one extent per block.
fn runs_from_pages(pages: BTreeMap<u64, Box<[u8; BLOCK_SIZE]>>) -> Vec<(u64, Vec<u8>)> {
    let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
    for (block, page) in pages {
        match runs.last_mut() {
            // Extend the current run if this block is the next one.
            Some((start, buf)) if *start + (buf.len() / BLOCK_SIZE) as u64 == block => {
                buf.extend_from_slice(&page[..]);
            }
            _ => runs.push((block, page.to_vec())),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_fetch(_: u64) -> [u8; BLOCK_SIZE] {
        [0u8; BLOCK_SIZE]
    }

    #[test]
    fn store_then_shadow_and_get() {
        let pc = PageCache::new();
        let data = vec![0xABu8; 3 * BLOCK_SIZE + 17];
        pc.store(7, 4096, &data, 99, zero_fetch);
        assert_eq!(pc.shadow(7), Some((4096 + data.len() as u64, 99)));
        let got = pc.get_pages(7, &[0, 1, 2, 3]);
        assert_eq!(got.len(), 3); // block 0 untouched
        assert!(got.iter().all(|(_, p)| p.iter().all(|&b| b == 0xAB)));
    }

    #[test]
    fn partial_write_merges_not_overwrites() {
        let pc = PageCache::new();
        // Write a full block of 1s.
        pc.store(3, 0, &[1u8; BLOCK_SIZE], 1, zero_fetch);
        // Overwrite bytes 100..150 in the SAME block with 2s.
        pc.store(3, 100, &[2u8; 50], 2, zero_fetch);
        let pages = pc.get_pages(3, &[0]);
        assert_eq!(pages.len(), 1);
        let p = pages[0].1;
        assert_eq!(p[99], 1);
        assert_eq!(p[100], 2);
        assert_eq!(p[149], 2);
        assert_eq!(p[150], 1);
    }

    #[test]
    fn rmw_fetches_committed_content() {
        let pc = PageCache::new();
        // "Committed" block: all 7s.
        let committed = [7u8; BLOCK_SIZE];
        pc.store(9, 100, &[5u8; 3], 1, |b| {
            assert_eq!(b, 0);
            committed
        });
        let p = pc.get_pages(9, &[0])[0].1;
        assert_eq!(p[99], 7);
        assert_eq!(p[100], 5);
        assert_eq!(p[102], 5);
        assert_eq!(p[103], 7);
    }

    #[test]
    fn drain_produces_contiguous_runs_and_clears() {
        let pc = PageCache::new();
        pc.store(5, 0, &[9u8; BLOCK_SIZE], 1, zero_fetch);
        pc.store(5, 2 * BLOCK_SIZE as u64, &[8u8; BLOCK_SIZE], 1, zero_fetch);
        pc.store(5, BLOCK_SIZE as u64, &[7u8; BLOCK_SIZE], 1, zero_fetch);
        let runs = pc.drain_runs(5);
        // Three blocks written out of order but all contiguous.
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 0);
        assert_eq!(runs[0].1.len(), 3 * BLOCK_SIZE);
        assert!(pc.shadow(5).is_none());
        assert!(pc.drain_runs(5).is_empty());
    }

    #[test]
    fn drop_ino_forgets_everything() {
        let pc = PageCache::new();
        pc.store(11, 0, &[1u8; BLOCK_SIZE], 1, zero_fetch);
        pc.drop_ino(11);
        assert!(pc.shadow(11).is_none());
        assert!(pc.get_pages(11, &[0]).is_empty());
        assert!(pc.dirty_inodes().is_empty());
    }

    #[test]
    fn same_ino_gate_serializes_writers() {
        let pc = PageCache::new();
        let gate = pc.gate(42);
        let g2 = pc.gate(42);
        let _held = gate.lock().unwrap();
        // While one writer holds the gate, another cannot enter.
        assert!(g2.try_lock().is_err());
        let g3 = pc.gate(43);
        assert!(g3.try_lock().is_ok()); // different inode: independent
    }
}
