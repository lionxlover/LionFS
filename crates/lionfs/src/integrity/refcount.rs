//! Refcount coverage tree (Phase 6 data-path CoW, completed in 3.2).
//!
//! Model: a B-tree keyed by `physical_start` whose values are
//! `[physical_start, length, count]` runs with the invariants:
//!
//! * runs are disjoint (no two entries overlap),
//! * every run has `count >= 1`,
//! * a run's count means "number of external references pinning every
//!   physical block of this run" -- references OTHER than the live
//!   filesystem's own extent mappings.
//!
//! Who pins:
//! * `create_snapshot` pins every extent run of every inode at the
//!   moment of the snapshot (the read view it points at stays intact
//!   because the write path refuses in-place modification of pinned
//!   blocks and redirects -- see `file::writer`).
//! * dedup sharing (`integrity::dedup`, 3.2) pins single blocks it
//!   maps into a second inode, for the same reason.
//!
//! What the write path does with it: before modifying a mapped block
//! in place, it asks `is_pinned(block)`. Pinned -> copy-on-write into
//! a fresh block and remap the extent. Not pinned -> in-place write,
//! exactly the pre-3.2 behavior with zero extra cost (the probe is one
//! `lookup_floor`, and the whole tree is empty until the first
//! snapshot / dedup share).
//!
//! Honesty note: this is coverage counting, not back-reference
//! tracking. When a snapshot is deleted, `delete_snapshot` walks the
//! snapshot's OWN inode tree (its roots are recorded in its
//! SnapshotRecord) and unpins exactly what it pinned, so pin release
//! is exact for snapshots. Truncating a pinned range does NOT free the
//! physical blocks (they are pinned); the free is deferred until the
//! pin count reaches zero -- the same lazy-free shape Btrfs uses for
//! shared extents.

use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use bytemuck::{Pod, Zeroable};
use std::io::{Error, ErrorKind, Result};

pub const REFCOUNT_TREE_NODE_TYPE: u32 = 7;

/// One coverage run: every physical block in
/// `[physical_start, physical_start + length)` is pinned `count` times.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct CoverageEntry {
    pub physical_start: u64,
    pub length: u64,
    pub count: u32,
    pub padding: u32,
}

pub struct RefCountManager {
    tree: BTree<u64, CoverageEntry>,
}

impl RefCountManager {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, REFCOUNT_TREE_NODE_TYPE),
        }
    }

    /// Initialize an empty coverage tree at `root_block`.
    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, CoverageEntry>::init_empty(ctx, root_block, REFCOUNT_TREE_NODE_TYPE)
    }

    /// The pin count covering one physical block (0 = unpinned, free to
    /// modify in place).
    pub fn coverage_at(&self, ctx: &mut TxContext, block: u64) -> Result<u32> {
        if let Some((start, entry)) = self.tree.lookup_floor(ctx, &block)? {
            if start == entry.physical_start && block < entry.physical_start + entry.length {
                return Ok(entry.count);
            }
        }
        Ok(0)
    }

    /// Whether the single physical block is pinned by any run.
    pub fn is_pinned(&self, ctx: &mut TxContext, block: u64) -> Result<bool> {
        Ok(self.coverage_at(ctx, block)? > 0)
    }

    /// Whether EVERY block of `[ps, ps+len)` is currently pinned.
    /// (Used by truncate's pin-aware free: a run may only be freed if
    /// it is entirely unpinned.)
    pub fn is_range_pinned(&self, ctx: &mut TxContext, ps: u64, len: u64) -> Result<bool> {
        if len == 0 {
            return Ok(false);
        }
        // Walk the runs overlapping [ps, ps+len) and require full
        // coverage. (iter_all is fine here: this runs on truncate /
        // free paths, not per-write.)
        let end = ps + len;
        let mut covered: u64 = 0;
        for (start, entry) in self.tree.iter_all(ctx)? {
            let e_start = start;
            let e_end = entry.physical_start + entry.length;
            if e_end <= ps || e_start >= end {
                continue;
            }
            let o_lo = e_start.max(ps);
            let o_hi = e_end.min(end);
            covered += o_hi - o_lo;
        }
        Ok(covered >= len)
    }

    /// Pin `[ps, ps+len)`: every block of the range gets +1. Existing
    /// runs are split at the pin's boundaries so the disjointness
    /// invariant survives partial overlaps; gaps inside the range
    /// become fresh runs with count 1.
    pub fn pin_range<F>(
        &mut self,
        ctx: &mut TxContext,
        ps: u64,
        len: u64,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if len == 0 {
            return Ok(());
        }
        let end = ps + len;

        // Snapshot the overlapping runs BEFORE mutating (iter_all
        // materializes the tree, so the iteration is stable).
        let overlaps: Vec<(u64, CoverageEntry)> = self
            .tree
            .iter_all(ctx)?
            .into_iter()
            .filter(|(start, entry)| {
                let e_end = entry.physical_start + entry.length;
                e_end > ps && *start < end
            })
            .collect();

        // Remove every overlapping run, then re-insert its pieces with
        // the overlapped middle bumped by one.
        for (start, _entry) in &overlaps {
            self.tree.remove(ctx, start)?;
        }
        for (start, entry) in &overlaps {
            let e_start = *start;
            let e_end = entry.physical_start + entry.length;
            // Left piece (before the pin window).
            if e_start < ps {
                self.tree.insert(
                    ctx,
                    e_start,
                    CoverageEntry {
                        physical_start: e_start,
                        length: ps - e_start,
                        count: entry.count,
                        padding: 0,
                    },
                    &mut *allocate_block,
                )?;
            }
            // Middle piece (inside the window): +1.
            let m_lo = e_start.max(ps);
            let m_hi = e_end.min(end);
            if m_hi > m_lo {
                self.tree.insert(
                    ctx,
                    m_lo,
                    CoverageEntry {
                        physical_start: m_lo,
                        length: m_hi - m_lo,
                        count: entry.count + 1,
                        padding: 0,
                    },
                    &mut *allocate_block,
                )?;
            }
            // Right piece (after the pin window).
            if e_end > end {
                self.tree.insert(
                    ctx,
                    end,
                    CoverageEntry {
                        physical_start: end,
                        length: e_end - end,
                        count: entry.count,
                        padding: 0,
                    },
                    &mut *allocate_block,
                )?;
            }
        }

        // Fill the gaps inside [ps, end) with fresh count-1 runs.
        let mut cursor = ps;
        // Overlapping runs are sorted by start (iter_all is in-order).
        let mut sorted: Vec<(u64, u64)> = overlaps
            .iter()
            .map(|(s, e)| (*s, e.physical_start + e.length))
            .collect();
        sorted.sort();
        for (s, e) in sorted {
            if s > cursor {
                self.tree.insert(
                    ctx,
                    cursor,
                    CoverageEntry {
                        physical_start: cursor,
                        length: (s - cursor).min(end - cursor),
                        count: 1,
                        padding: 0,
                    },
                    &mut *allocate_block,
                )?;
            }
            cursor = cursor.max(e);
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            self.tree.insert(
                ctx,
                cursor,
                CoverageEntry {
                    physical_start: cursor,
                    length: end - cursor,
                    count: 1,
                    padding: 0,
                },
                allocate_block,
            )?;
        }
        Ok(())
    }

    /// Unpin `[ps, ps+len)`: every block of the range gets -1; runs
    /// whose count reaches zero are removed. Unpinning a range that
    /// was never pinned is an error (the caller is unpinned something
    /// it did not pin -- a bookkeeping bug we want loud, not silent).
    pub fn unpin_range<F>(
        &mut self,
        ctx: &mut TxContext,
        ps: u64,
        len: u64,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if len == 0 {
            return Ok(());
        }
        let end = ps + len;

        let overlaps: Vec<(u64, CoverageEntry)> = self
            .tree
            .iter_all(ctx)?
            .into_iter()
            .filter(|(start, entry)| {
                let e_end = entry.physical_start + entry.length;
                e_end > ps && *start < end
            })
            .collect();

        // VALIDATE FIRST, mutate second: an unbalanced unpin must leave
        // the tree exactly as it was (the caller treats the error as
        // "nothing happened").
        let covered: u64 = overlaps
            .iter()
            .map(|(s, e)| {
                let e_end = e.physical_start + e.length;
                (e_end.min(end)).saturating_sub((*s).max(ps))
            })
            .sum();
        if overlaps.is_empty() || covered < len {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "unpin_range: range not fully pinned (unbalanced pin/unpin)",
            ));
        }

        for (start, _entry) in &overlaps {
            self.tree.remove(ctx, start)?;
        }
        for (start, entry) in &overlaps {
            let e_start = *start;
            let e_end = entry.physical_start + entry.length;
            if e_start < ps {
                self.tree.insert(
                    ctx,
                    e_start,
                    CoverageEntry {
                        physical_start: e_start,
                        length: ps - e_start,
                        count: entry.count,
                        padding: 0,
                    },
                    &mut *allocate_block,
                )?;
            }
            let m_lo = e_start.max(ps);
            let m_hi = e_end.min(end);
            if m_hi > m_lo {
                let new_count = entry.count - 1;
                if new_count > 0 {
                    self.tree.insert(
                        ctx,
                        m_lo,
                        CoverageEntry {
                            physical_start: m_lo,
                            length: m_hi - m_lo,
                            count: new_count,
                            padding: 0,
                        },
                        &mut *allocate_block,
                    )?;
                }
                // count == 0: the run disappears -- the blocks are no
                // longer pinned by anyone.
            }
            if e_end > end {
                self.tree.insert(
                    ctx,
                    end,
                    CoverageEntry {
                        physical_start: end,
                        length: e_end - end,
                        count: entry.count,
                        padding: 0,
                    },
                    &mut *allocate_block,
                )?;
            }
        }
        // Full coverage was validated up front; the reassembly above
        // is total. Done.
        Ok(())
    }

    /// All runs, in physical order (diagnostics, tests, GC).
    pub fn iter_coverage(&self, ctx: &mut TxContext) -> Result<Vec<CoverageEntry>> {
        Ok(self.tree.iter_all(ctx)?.into_iter().map(|(_, v)| v).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::block_io::Disk;
    use crate::ondisk::serialization::Superblock;
    use crate::transaction::manager::TransactionManager;

    fn zero_sb() -> Superblock {
        Superblock {
            block_size: 4096,
            next_ino: 2,
            ..unsafe { std::mem::zeroed() }
        }
    }

    #[test]
    fn pin_and_query_round_trip() {
        let path = std::env::temp_dir().join("test_rc_pin.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = zero_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);
        RefCountManager::init_empty(&mut ctx, 100).unwrap();
        let mut rc = RefCountManager::new(100);
        let mut next = 200u64;
        let mut alloc = move |_c: &mut TxContext| {
            let b = next;
            next += 1;
            Ok(b)
        };
        rc.pin_range(&mut ctx, 100, 10, &mut alloc).unwrap();
        assert!(rc.is_pinned(&mut ctx, 100).unwrap());
        assert!(rc.is_pinned(&mut ctx, 109).unwrap());
        assert!(!rc.is_pinned(&mut ctx, 99).unwrap());
        assert!(!rc.is_pinned(&mut ctx, 110).unwrap());
        assert_eq!(rc.coverage_at(&mut ctx, 105).unwrap(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn double_pin_counts_two_and_single_unpin_leaves_one() {
        let path = std::env::temp_dir().join("test_rc_double.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = zero_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);
        RefCountManager::init_empty(&mut ctx, 100).unwrap();
        let mut rc = RefCountManager::new(100);
        let mut next = 200u64;
        let mut alloc = move |_c: &mut TxContext| {
            let b = next;
            next += 1;
            Ok(b)
        };
        rc.pin_range(&mut ctx, 50, 5, &mut alloc).unwrap();
        rc.pin_range(&mut ctx, 50, 5, &mut alloc).unwrap();
        assert_eq!(rc.coverage_at(&mut ctx, 52).unwrap(), 2);
        rc.unpin_range(&mut ctx, 50, 5, &mut alloc).unwrap();
        assert_eq!(rc.coverage_at(&mut ctx, 52).unwrap(), 1);
        rc.unpin_range(&mut ctx, 50, 5, &mut alloc).unwrap();
        assert_eq!(rc.coverage_at(&mut ctx, 52).unwrap(), 0);
        assert_eq!(rc.iter_coverage(&mut ctx).unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overlapping_pin_splits_runs_correctly() {
        let path = std::env::temp_dir().join("test_rc_split.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = zero_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);
        RefCountManager::init_empty(&mut ctx, 100).unwrap();
        let mut rc = RefCountManager::new(100);
        let mut next = 200u64;
        let mut alloc = move |_c: &mut TxContext| {
            let b = next;
            next += 1;
            Ok(b)
        };
        rc.pin_range(&mut ctx, 100, 10, &mut alloc).unwrap();
        rc.pin_range(&mut ctx, 105, 10, &mut alloc).unwrap();
        assert_eq!(rc.coverage_at(&mut ctx, 104).unwrap(), 1);
        assert_eq!(rc.coverage_at(&mut ctx, 105).unwrap(), 2);
        assert_eq!(rc.coverage_at(&mut ctx, 109).unwrap(), 2);
        assert_eq!(rc.coverage_at(&mut ctx, 110).unwrap(), 1);
        assert_eq!(rc.coverage_at(&mut ctx, 114).unwrap(), 1);
        assert_eq!(rc.coverage_at(&mut ctx, 115).unwrap(), 0);
        let runs = rc.iter_coverage(&mut ctx).unwrap();
        assert_eq!(runs.len(), 3, "expected exactly 3 disjoint runs");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unbalanced_unpin_is_an_error() {
        let path = std::env::temp_dir().join("test_rc_unbal.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = zero_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);
        RefCountManager::init_empty(&mut ctx, 100).unwrap();
        let mut rc = RefCountManager::new(100);
        let mut next = 200u64;
        let mut alloc = move |_c: &mut TxContext| {
            let b = next;
            next += 1;
            Ok(b)
        };
        assert!(rc.unpin_range(&mut ctx, 10, 5, &mut alloc).is_err());
        rc.pin_range(&mut ctx, 10, 5, &mut alloc).unwrap();
        assert!(rc.unpin_range(&mut ctx, 8, 10, &mut alloc).is_err());
        assert_eq!(rc.coverage_at(&mut ctx, 12).unwrap(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn range_fills_gaps_with_count_one_runs() {
        let path = std::env::temp_dir().join("test_rc_gaps.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = zero_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);
        RefCountManager::init_empty(&mut ctx, 100).unwrap();
        let mut rc = RefCountManager::new(100);
        let mut next = 200u64;
        let mut alloc = move |_c: &mut TxContext| {
            let b = next;
            next += 1;
            Ok(b)
        };
        rc.pin_range(&mut ctx, 500, 7, &mut alloc).unwrap();
        assert!(rc.is_range_pinned(&mut ctx, 500, 7).unwrap());
        assert!(!rc.is_range_pinned(&mut ctx, 500, 8).unwrap());
        assert!(!rc.is_range_pinned(&mut ctx, 499, 8).unwrap());
        let _ = std::fs::remove_file(&path);
    }
}
