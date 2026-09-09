//! `VfsOps` implementation for the core `LionFS` engine: the
//! platform-neutral operations surface (RFC-003). Every platform bridge
//! (FUSE on Linux/macOS, WinFsp on Windows) drives the file system
//! through exactly this impl, so semantics are identical everywhere by
//! construction rather than by porting discipline.
//!
//! Ported 1:1 from the 1.x `impl fuser::Filesystem for LionFS` -- the
//! method bodies are the same logic; the `reply.*` callback pattern
//! became `Result` returns and the libc errno constants became
//! `pal::posix` constants (the same ABI values).
//!
//! ## Phase 10: `&self` operations + parallel write intake
//!
//! Every method now takes `&self` (init/destroy keep `&mut`: they are
//! mount lifecycle). The single `&mut self` of 3.3 was the whole-FS
//! write lock; it is replaced by the explicit lock ladder in
//! [`crate::fs::filesystem::SharedCore`]:
//!
//! * **Reads** (`lookup`/`getattr`/`readdir`/`read`/...) take NO
//!   global lock. They snapshot the superblock (`Copy`), hit the
//!   concurrent inode cache, consult the page cache, and read the
//!   disk. Only when a staging transaction is actually in flight do
//!   they briefly take the staging lock to read through its overlay
//!   (the exact 3.3 visibility semantics).
//! * **Buffered writes** land in the write-back page cache
//!   ([`crate::fs::page_cache`]) behind a per-inode gate and return;
//!   N threads writing N files proceed in parallel -- this is the
//!   Phase 10 headline.
//! * **Staging** (`flush_ino`, and every metadata op: create, unlink,
//!   setattr, ...) holds the `active_tx` staging lock and runs the
//!   UNCHANGED 3.3 machinery through one transaction, so every
//!   single-writer invariant (B-trees, allocator, CoW, dedup,
//!   journal) is preserved by construction.
//!
//! Compressed inodes bypass the page cache (cluster path is
//! stateful); `LFS_WRITEBACK=0` restores 3.3 write-through for A/B
//! measurement and as a conservative escape hatch.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::filesystem::LionFS;
use crate::ondisk::serialization::{Inode, BLOCK_SIZE};
use crate::pal::posix;
use crate::transaction::transaction::TxContext;
use crate::vfs::{
    VfsAttr, VfsCreate, VfsDirEntry, VfsError, VfsKind, VfsOps, VfsResult, VfsSetAttr, VfsStatFs,
};

/// Lock a mutex without panicking on poisoning (see the twin in
/// `filesystem.rs` for the rationale).
fn lock_ok<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Virtual control files exposed at the mount root (1.x behavior,
/// preserved: scrub status and health report as read-only files).
const SCRUB_INO: u64 = 999_999;
const HEALTH_INO: u64 = 999_998;

impl LionFS {
    /// Maps an on-disk inode into the bridge-neutral attribute shape.
    fn to_vfs_attr(&self, inode: &Inode) -> VfsAttr {
        let kind = if posix::is_dir(inode.mode) {
            VfsKind::Directory
        } else {
            VfsKind::RegularFile
        };
        let t = |secs: i64| -> SystemTime {
            if secs >= 0 {
                UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
            } else {
                UNIX_EPOCH
            }
        };
        VfsAttr {
            ino: inode.ino,
            size: inode.size,
            blocks: inode.size.div_ceil(BLOCK_SIZE as u64),
            kind,
            perm: inode.mode & 0o7777,
            nlink: inode.links_count,
            uid: inode.uid,
            gid: inode.gid,
            atime: t(inode.atime),
            mtime: t(inode.mtime),
            ctime: t(inode.ctime),
            blksize: BLOCK_SIZE as u32,
            flags: 0,
        }
    }

    /// `getattr`-shaped attribute with the page-cache shadow applied
    /// (buffered writes extend size / refresh mtime immediately, the
    /// same contract a kernel page cache gives).
    fn attr_with_shadow(&self, inode: &Inode) -> VfsAttr {
        let mut attr = self.to_vfs_attr(inode);
        if let Some((shadow_size, shadow_mtime)) = self.core.page_cache.shadow(inode.ino) {
            if shadow_size > attr.size {
                attr.size = shadow_size;
                attr.blocks = shadow_size.div_ceil(BLOCK_SIZE as u64);
            }
            if shadow_mtime > 0 {
                attr.mtime = UNIX_EPOCH + std::time::Duration::from_secs(shadow_mtime);
            }
        }
        attr
    }

    /// Read-only context: reads through the in-flight transaction's
    /// overlay when one exists (3.3 visibility), else the lock-free
    /// scratch read. Phase 11: the in-flight probe is an atomic flag
    /// (`tx_present`), NOT the staging Mutex -- N readers never ping
    /// the lock; the scratch path is `with_scratch_ctx` (seqlock +
    /// pending-overlay retry, no staging lock at all).
    fn with_read_ctx<R>(
        &self,
        mut f: impl FnMut(&mut TxContext<'_>, &crate::ondisk::serialization::Superblock) -> R,
    ) -> R {
        let core = &self.core;
        // 3.4 reader profile (Phase 11 postmortem: the tx_present
        // atomic fast probe was removed while root-causing a live
        // mixed-epoch read race under heavy parallel load; the direct
        // lock probe is the conservative form and was proven by the
        // 3.4 suite): take the staging lock, read through the active
        // transaction's overlay if one exists, else the lock-free
        // seqlock + pending-overlay scratch read.
        let mut guard = lock_ok(&core.active_tx);
        if let Some(tx) = guard.as_mut() {
            let sb = core.sb();
            let pending = core.snapshot_committing();
            let out = {
                let mut ctx = TxContext::new(&core.disk, tx).with_pending(&pending);
                f(&mut ctx, &sb)
            };
            return out;
        }
        drop(guard);
        core.with_scratch_ctx(|ctx, sb| f(ctx, sb))
    }

    /// Staging context: begin-or-continue the shared transaction under
    /// the staging lock and run `f`. Returns `(result, dirty_blocks)`
    /// so each caller can apply its own commit policy exactly as 3.3
    /// did (`write`: commit at 1024 dirty blocks; create/unlink/...:
    /// commit at the end of the op).
    fn with_stage_ctx<R>(
        &self,
        now: u64,
        f: impl FnOnce(
            &mut TxContext<'_>,
            &crate::ondisk::serialization::Superblock,
        ) -> R,
    ) -> (R, usize) {
        let (out, dirty, _txid) = self.with_stage_ctx_marked(now, f);
        (out, dirty)
    }

    /// Staging context that also returns the identity of the
    /// transaction the staging ran in (Phase 11 group-coverage fix:
    /// `commit_until_covered` waits for THAT transaction to retire --
    /// the 3.4 epoch-mark reasoning no longer holds once commits
    /// increment their epoch after the I/O instead of under the
    /// staging lock).
    ///
    /// Phase 11: the context sees the pending frozen groups (staging
    /// builds ON TOP of a group in flight to disk, never on pre-quiesce
    /// state -- the lost-update guard), and the committer thread gets
    /// a wake hint so staged bytes drain without a local drive.
    fn with_stage_ctx_marked<R>(
        &self,
        now: u64,
        f: impl FnOnce(
            &mut TxContext<'_>,
            &crate::ondisk::serialization::Superblock,
        ) -> R,
    ) -> (R, usize, Option<u64>) {
        let core = &self.core;
        let mut guard = lock_ok(&core.active_tx);
        if guard.is_none() {
            *guard = Some(core.tx_manager.begin(now));
            core.tx_present.store(true, Ordering::Release);
        }
        let tx = guard.as_mut().expect("just began");
        let txid = tx.id;
        let sb = core.sb();
        let pending = core.snapshot_committing();
        let out = {
            let mut ctx = TxContext::new(&core.disk, tx)
                .with_cow_barrier(sb.last_snapshot_generation)
                .with_pending(&pending);
            f(&mut ctx, &sb)
        };
        let dirty = guard.as_ref().map(|t| t.dirty_blocks.len()).unwrap_or(0);
        drop(guard);
        let txid = if dirty > 0 { Some(txid) } else { None };
        // Deliberately NO committer wake here: an fsync-driven commit
        // self-drives (`commit_until_covered`), and a committer thread
        // racing the quiesce would put a thread handoff back on the
        // critical path. The committer's 20 ms poll still drains
        // staging that no fsync drives (metadata ops sitting in the
        // active transaction).
        (out, dirty, txid)
    }

    /// Phase 11 group commit, adaptive: `mark = commit_end` sampled
    /// under the staging lock, then drive until a commit covers it.
    /// The policy is the pipeline's whole point:
    ///
    /// * **No commit in flight -> drive it OURSELF, now.** A quiesce
    ///   is microseconds and the I/O phases hold no staging lock, so
    ///   the caller's own commit never blocks the next writer's
    ///   staging. Single-stream fsync pays zero handoff latency (a
    ///   thread wake-up round trip costs more than the quiesce).
    /// * **A commit in flight -> WAIT for it.** Our bytes were staged
    ///   while its group was mid-I/O (the overlap is the win); its
    ///   driver -- or the committer thread -- loops straight into the
    ///   next group, which carries our bytes: N concurrent fsyncs pay
    ///   one journal run and one sync pair, not N. Racing self-drives
    ///   are harmless: `commit_one`'s quiesce hands the transaction to
    ///   exactly one driver; the loser re-checks and waits.
    ///
    /// The background committer thread remains the drain for
    /// non-fsync staging (threshold flushes, write-through) and a
    /// waiter of last resort; with this policy it is never on an
    /// fsync's critical path.
    fn commit_until_covered(&self, txids: &[u64]) -> bool {
        let core = &self.core;
        if txids.is_empty() {
            return true; // nothing live under our name: clean fsync
        }
        let covered = |core: &crate::fs::filesystem::SharedCore| {
            txids.iter().all(|t| !core.tx_live(*t))
        };
        let mut patience: u32 = 0;
        loop {
            if covered(core) {
                // Every group carrying our bytes retired: journal +
                // syncs + apply + root cells all landed.
                return true;
            }
            let in_flight = !core.committing_is_empty();
            if !in_flight {
                // Idle: self-drive. (If another driver races us, one of
                // the two `commit_one` calls wins the quiesce; the
                // loser re-checks and waits for the winner's retire.)
                if core.commit_one() {
                    continue;
                }
                // Nothing stageable: a racing driver is between take
                // and push, or already retired between our checks.
                if covered(core) {
                    return true;
                }
            }
            if patience < 100 {
                patience += 1;
                let _coord = lock_ok(&core.commit_coord);
                let (_guard, _timed_out) = core
                    .commit_done
                    .wait_timeout(_coord, Duration::from_millis(5))
                    .unwrap_or_else(|p| p.into_inner());
                continue;
            }
            // Patience exhausted (a pathological stalled commit):
            // last-resort self-drive.
            if core.commit_one() {
                patience = 0;
                continue;
            }
            return covered(core);
        }
    }

    /// Effective size of `ino`: the committed size grown to the
    /// write-back shadow (read-your-own-buffered-write).
    fn effective_size(&self, inode: &Inode) -> u64 {
        self.core
            .page_cache
            .shadow(inode.ino)
            .map_or(inode.size, |(s, _)| inode.size.max(s))
    }

    /// Fetch the current plaintext content of one logical block for
    /// the page-cache RMW path (partial overwrite of a block that is
    /// not yet cached). Reads through the standard read path so
    /// staged bytes, checksums, and cipher decoding are all honored.
    /// Beyond-EOF and unallocated blocks come back as zeros.
    fn fetch_block(&self, inode: &Inode, block: u64) -> [u8; BLOCK_SIZE] {
        let mut out = [0u8; BLOCK_SIZE];
        let read = self.with_read_ctx(|ctx, sb| {
            let cctx = self.core.resolve_block_cipher_ctx(ctx, inode).ok()?;
            crate::file::writer::FileManager::read_file(
                ctx,
                sb.checksum_tree_root,
                sb.bad_blocks_root,
                &cctx,
                &mut inode.clone(),
                block * BLOCK_SIZE as u64,
                BLOCK_SIZE as u64,
            )
            .ok()
        });
        if let Some(data) = read {
            let n = data.len().min(BLOCK_SIZE);
            out[..n].copy_from_slice(&data[..n]);
        }
        out
    }

    /// Phase 10: drain one inode's cached pages through the unchanged
    /// staging machinery. Contiguous runs become single large
    /// `write_file` calls, so the speculative-run allocator lays down
    /// whole extents per flush. Lock order: per-inode gate -> staging
    /// lock (never the reverse).
    pub(crate) fn flush_ino(&self, ino: u64) -> std::io::Result<()> {
        let gate = self.core.page_cache.gate(ino);
        let _gate_guard = gate.lock().unwrap();
        self.flush_ino_locked(ino)
    }

    /// `flush_ino` that also returns the identity of the transaction
    /// the flushed bytes were staged into (for
    /// [`Self::commit_until_covered`]): `None` when there was nothing
    /// to flush (a clean fsync -- nothing to cover).
    /// `flush_ino` that also returns the transaction ids whose
    /// retirement covers the flushed bytes (for
    /// [`Self::commit_until_covered`]): the transaction the staging
    /// ran in when pages were drained; when there was nothing
    /// page-cached, EVERY currently-live group (write-through staging
    /// -- compressed inodes, `LFS_WRITEBACK=0` -- can sit in the
    /// active transaction OR in a group another thread already
    /// quiesced); empty when nothing is live at all (a clean fsync).
    pub(crate) fn flush_ino_marked(&self, ino: u64) -> std::io::Result<Vec<u64>> {
        let flush_gate = self.core.page_cache.flush_gate(ino);
        let _flush_guard = flush_gate.lock().unwrap();

        let (runs, drained_eof) = {
            let gate = self.core.page_cache.gate(ino);
            let _gate_guard = gate.lock().unwrap();
            if !self.core.page_cache.has_pages(ino) {
                return Ok(self.core.live_txids());
            }
            let (runs, drained_eof) = self.core.page_cache.drain_runs_and_eof(ino);
            if runs.is_empty() {
                return Ok(self.core.live_txids());
            }
            (runs, drained_eof)
        };
        let now = now_secs();
        let core = &self.core;
        let cached_size = core.inode_cache.get(ino).map(|i| i.size);
        let (result, _dirty, txid) =
            self.with_stage_ctx_marked(now, |ctx, sb| -> std::io::Result<()> {
                let bg_desc = core.get_bg_desc();
                let blocks_per_group = sb.blocks_per_group;
                let mut inode =
                    crate::inode::manager::InodeManager::read_inode(ctx, sb.inode_tree_root, ino)?;
                // Permanent tripwire (caught live in the Phase 11
                // commit-ordering hunt): the tree-read must never be
                // older than the last flush's cache insert -- a smaller
                // size here means a lost update is about to be
                // committed.
                if let Some(cs) = cached_size {
                    debug_assert!(
                        inode.size >= cs,
                        "stale inode tree-read: ino {ino} tree {} < cached {cs}",
                        inode.size
                    );
                }
                let cctx = core.resolve_block_cipher_ctx(ctx, &inode)?;
                let pre_flush_size = inode.size;
                crate::file::writer::FileManager::write_file_runs(
                    ctx,
                    &bg_desc,
                    blocks_per_group,
                    sb.checksum_tree_root,
                    sb.refcount_tree_root,
                    sb.dedupe_tree_root,
                    &cctx,
                    &mut inode,
                    &runs,
                )?;
                // Phase 12 fix: block-granular storage, LOGICAL size.
                // write_file saw whole pages; the file ends at
                // max(pre-flush committed size, the shadow EOF) -- the
                // shadow covers only THIS flush's buffered bytes, and
                // committed content the pages merely RMW-merged must
                // not be truncated away.
                {
                    let mut logical = pre_flush_size;
                    if let Some(eof) = drained_eof {
                        logical = logical.max(eof);
                    }
                    inode.size = logical;
                }
                inode.mtime = now as i64;
                crate::inode::manager::InodeManager::write_inode_with_allocator(
                    ctx,
                    sb.inode_tree_root,
                    &inode,
                    |c| {
                        crate::allocator::bitmap::Allocator::allocate_extents(
                            c,
                            &bg_desc,
                            blocks_per_group,
                            1,
                        )
                    },
                )?;
                core.inode_cache.insert(ino, inode, false);
                Ok(())
            });
        result?;
        // The flushed bytes ran into the transaction the staging context
        // reports (the runs were nonempty, so it staged).
        Ok(txid.into_iter().collect())
    }

    /// `flush_ino` for callers that ALREADY hold `ino`'s write gate
    /// (the intake path's threshold flush -- the dirty victim is
    /// usually the file being written, and re-taking its own gate
    /// would self-deadlock).
    pub(crate) fn flush_ino_locked(&self, ino: u64) -> std::io::Result<()> {
        if !self.core.page_cache.has_pages(ino) {
            return Ok(());
        }
        let (runs, drained_eof) = self.core.page_cache.drain_runs_and_eof(ino);
        if runs.is_empty() {
            return Ok(());
        }
        let now = now_secs();
        let core = &self.core;
        let (result, dirty) = self.with_stage_ctx(now, |ctx, sb| -> std::io::Result<()> {
            let bg_desc = core.get_bg_desc();
            let blocks_per_group = sb.blocks_per_group;
            // Read the inode from the staging overlay (TxContext dirty_blocks map) first,
            // falling back to the B-tree. The transaction's own dirty overlay already
            // serves recently-written inode tree nodes from RAM, so this is typically
            // O(tree_height) HashMap lookups, not disk reads.
            let mut inode =
                crate::inode::manager::InodeManager::read_inode(ctx, sb.inode_tree_root, ino)?;
            let cctx = core.resolve_block_cipher_ctx(ctx, &inode)?;
            let pre_flush_size = inode.size;
            crate::file::writer::FileManager::write_file_runs(
                ctx,
                &bg_desc,
                blocks_per_group,
                sb.checksum_tree_root,
                sb.refcount_tree_root,
                sb.dedupe_tree_root,
                &cctx,
                &mut inode,
                &runs,
            )?;
            // Phase 12 fix (same as flush_ino_marked): logical size is
            // max(pre-flush committed size, the shadow EOF).
            {
                let mut logical = pre_flush_size;
                if let Some(eof) = drained_eof {
                    logical = logical.max(eof);
                }
                inode.size = logical;
            }
            inode.mtime = now as i64;
            crate::inode::manager::InodeManager::write_inode_with_allocator(
                ctx,
                sb.inode_tree_root,
                &inode,
                |c| {
                    crate::allocator::bitmap::Allocator::allocate_extents(
                        c,
                        &bg_desc,
                        blocks_per_group,
                        1,
                    )
                },
            )?;
            core.inode_cache.insert(ino, inode, false);
            Ok(())
        });
        result?;
        // Commit threshold (Btrfs / ZFS group-commit model): a commit is
        // only triggered after enough blocks have accumulated in the active
        // transaction. Committing too often forces a full journal write +
        // fdatasync pair for each small batch of rand-writes; waiting
        // longer amortizes that overhead across more blocks.
        // 8192 blocks × 4 KiB = 32 MiB per group -- matches the new
        // FLUSH_SOFT_LIMIT window and keeps journal pressure low.
        if dirty >= 1024 {
            self.core.commit_one();
        }
        Ok(())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn e(errno: i32) -> VfsError {
    VfsError::new(errno)
}

impl VfsOps for LionFS {
    fn init(&mut self) {
        let bg = self.core.get_bg_desc();
        let core = Arc::clone(&self.core);
        self.scrubber.start(core, bg, self.image_path.clone());
    }

    fn destroy(&mut self) {
        // Phase 11: unmount is a full barrier -- stop the committer
        // thread first (so no commit races the teardown), flush every
        // buffered page through staging, drive every remaining group
        // to retirement, then sync. Nothing dirty survives `destroy`
        // silently.
        crate::fs::flusher::FlusherWorker::stop(&self.core);
        self.core.stop_committer();
        for ino in self.core.page_cache.dirty_inodes() {
            let _ = self.flush_ino(ino);
        }
        while self.core.commit_one() {}
        self.scrubber.stop();
        // 3.6 (Format Vault): unmount is a CHECKPOINT. Slot writes are
        // otherwise tied to root moves (3.5 economics), so the last few
        // allocations' space accounting could sit uncommitted until the
        // next root move; persisting the full current superblock here
        // makes statfs, the conformance battery, and the next mount see
        // exact accounting at every clean shutdown.
        {
            let sb = self.core.sb();
            if let Err(err) =
                crate::ondisk::superblock::write_all_slots(&self.core.disk, &sb, sb.generation)
            {
                eprintln!("Failed to checkpoint superblock on unmount: {err}");
            }
        }
        if let Err(err) = self.core.disk.sync() {
            eprintln!("Failed to sync disk on unmount: {err}");
        }
    }

    fn lookup(&self, parent: u64, name: &str) -> VfsResult<VfsAttr> {
        if parent == 1 {
            if name == ".lfs_scrub" {
                return Ok(Self::virtual_attr(SCRUB_INO));
            } else if name == ".lfs_health" {
                return Ok(Self::virtual_attr(HEALTH_INO));
            }
        }

        // Fast path: dentry cache hit (O(1) memory lookup)
        if let Some(&cached_ino) = self.core.dentry_cache.read().unwrap().get(&(parent, name.to_string())) {
            if let Ok(inode) = self.core.get_inode(cached_ino) {
                return Ok(self.to_vfs_attr(&inode));
            }
        }

        let found: Option<Inode> = self.with_read_ctx(|ctx, sb| {
            if let Ok(mut parent_inode) = crate::inode::manager::InodeManager::read_inode(
                ctx,
                sb.inode_tree_root,
                parent,
            ) {
                if let Ok(entries) = crate::directory::entries::DirManager::read_entries(
                    ctx,
                    sb.checksum_tree_root,
                    sb.bad_blocks_root,
                    &mut parent_inode,
                ) {
                    for entry in entries {
                        if entry.name == name {
                            if let Ok(inode) = crate::inode::manager::InodeManager::read_inode(
                                ctx,
                                sb.inode_tree_root,
                                entry.ino,
                            ) {
                                return Some(inode);
                            }
                        }
                    }
                }
            }
            None
        });
        match found {
            Some(inode) => {
                self.core.dentry_cache.write().unwrap().insert((parent, name.to_string()), inode.ino);
                Ok(self.to_vfs_attr(&inode))
            }
            None => Err(e(posix::ENOENT)),
        }
    }

    fn getattr(&self, ino: u64) -> VfsResult<VfsAttr> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Ok(Self::virtual_attr(ino));
        }
        self.core
            .get_inode(ino)
            .map(|inode| self.attr_with_shadow(&inode))
            .map_err(|_| e(posix::ENOENT))
    }

    fn setattr(&self, ino: u64, attr: &VfsSetAttr) -> VfsResult<VfsAttr> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Ok(Self::virtual_attr(ino));
        }

        let now = now_secs() as i64;
        let mut result_inode = None;

        // Phase 10: a size change must see the file's FULL length, so
        // the buffered pages are flushed through staging first. A
        // truncate that races the flush would otherwise cut a length
        // the cache still holds (and reintroduce a lost-update).
        if attr.size.is_some() {
            let _ = self.flush_ino(ino);
        }

        {
            let (r, _dirty) = self.with_stage_ctx(now as u64, |ctx, sb| {
                let bg_desc = self.core.get_bg_desc();
                let blocks_per_group = sb.blocks_per_group;
                let inode_tree_root = sb.inode_tree_root;
                if let Ok(mut inode) =
                    crate::inode::manager::InodeManager::read_inode(ctx, inode_tree_root, ino)
                {
                    if let Some(new_size) = attr.size {
                        let _ = crate::file::writer::FileManager::truncate_file(
                            ctx,
                            &bg_desc,
                            blocks_per_group,
                            sb.refcount_tree_root,
                            sb.checksum_tree_root,
                            &mut inode,
                            new_size,
                        );
                    }
                    crate::fs::metadata::apply_attr_changes(
                        &mut inode,
                        crate::fs::metadata::AttrChanges {
                            mode: attr.mode,
                            uid: attr.uid,
                            gid: attr.gid,
                            atime: attr
                                .atime
                                .map(|t| crate::fs::metadata::TimeOrNow::At(secs_of(t))),
                            mtime: attr
                                .mtime
                                .map(|t| crate::fs::metadata::TimeOrNow::At(secs_of(t))),
                        },
                        now,
                    );

                    // 3.6 (ACLs): a chmod on an ACL-bearing inode
                    // remaps the ACL's class entries (draft 17) and
                    // keeps the mode bits ACL-derived, in the same
                    // transaction.
                    if let Some(new_mode) = attr.mode {
                        if let Some(acl) =
                            self.read_acl_in_ctx(ctx, sb, ino, crate::security::posix_acl::ACL_ACCESS_XATTR)
                        {
                            let updated = acl.apply_chmod(new_mode);
                            let bg = self.core.get_bg_desc();
                            let mut allocate = |c: &mut TxContext| {
                                crate::allocator::bitmap::Allocator::allocate_extents_meta(
                                    c,
                                    &bg,
                                    sb.blocks_per_group,
                                    1,
                                )
                            };
                            if let Ok((root, to_free)) = crate::fs::xattrs::set_xattr(
                                ctx,
                                sb.xattr_tree_root,
                                ino,
                                crate::security::posix_acl::ACL_ACCESS_XATTR,
                                &updated.encode(),
                                0,
                                false,
                                &mut allocate,
                            ) {
                                for b in to_free {
                                    let _ = crate::allocator::bitmap::Allocator::free_extents(
                                        ctx, &bg, b, 1,
                                    );
                                }
                                if root != sb.xattr_tree_root {
                                    self.core.sb_write().xattr_tree_root = root;
                                }
                                inode.mode = (inode.mode & !0o777) | updated.mode_bits();
                            }
                        }
                    }

                    if crate::inode::manager::InodeManager::write_inode_with_allocator(
                        ctx,
                        inode_tree_root,
                        &inode,
                        |c| {
                            crate::allocator::bitmap::Allocator::allocate_extents(
                                c,
                                &bg_desc,
                                blocks_per_group,
                                1,
                            )
                        },
                    )
                    .is_ok()
                    {
                        result_inode = Some(inode);
                    }
                }
            });
            let (_r, _dirty) = (r, _dirty);
        }

        if let Some(final_inode) = result_inode {
            self.core.commit_one();
            self.core.inode_cache.insert(ino, final_inode, false);
            Ok(self.to_vfs_attr(&final_inode))
        } else {
            Err(e(posix::ENOENT))
        }
    }

    fn readdir(
        &self,
        ino: u64,
        offset: u64,
        max_entries: usize,
    ) -> VfsResult<Vec<VfsDirEntry>> {
        let out = self.with_read_ctx(|ctx, sb| {
            if let Ok(mut inode) = crate::inode::manager::InodeManager::read_inode(
                ctx,
                sb.inode_tree_root,
                ino,
            ) {
                if let Ok(entries) = crate::directory::entries::DirManager::read_entries(
                    ctx,
                    sb.checksum_tree_root,
                    sb.bad_blocks_root,
                    &mut inode,
                ) {
                    let mut dir_entries = vec![
                        (inode.ino, VfsKind::Directory, ".".to_string()),
                        // Simplified parent for Phase 1 (1.x behavior).
                        (inode.ino, VfsKind::Directory, "..".to_string()),
                    ];
                    for entry in entries {
                        let kind = if entry.file_type == 2 {
                            VfsKind::Directory
                        } else {
                            VfsKind::RegularFile
                        };
                        dir_entries.push((entry.ino, kind, entry.name));
                    }
                    if ino == 1 {
                        dir_entries.push((
                            SCRUB_INO,
                            VfsKind::RegularFile,
                            ".lfs_scrub".to_string(),
                        ));
                        dir_entries.push((
                            HEALTH_INO,
                            VfsKind::RegularFile,
                            ".lfs_health".to_string(),
                        ));
                    }
                    // The FUSE readdir offset protocol: each entry carries
                    // its 1-based next offset; callers start at the offset
                    // of the last entry they consumed.
                    return Some(
                        dir_entries
                            .into_iter()
                            .enumerate()
                            .skip(offset as usize)
                            .take(max_entries)
                            .map(|(i, (ino, kind, name))| VfsDirEntry {
                                ino,
                                kind,
                                name,
                                next_offset: (i + 1) as u64,
                            })
                            .collect::<Vec<_>>(),
                    );
                }
            }
            None
        });
        out.ok_or_else(|| e(posix::ENOENT))
    }

    fn read(&self, ino: u64, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        // Virtual control files first (1.x behavior).
        if ino == SCRUB_INO || ino == HEALTH_INO {
            let data = if ino == SCRUB_INO {
                self.scrubber.get_status().into_bytes()
            } else {
                self.with_read_ctx(|ctx, sb| {
                    crate::integrity::bad_blocks::BadBlockManager::get_health_report(
                        ctx,
                        sb.bad_blocks_root,
                    )
                    .into_bytes()
                })
            };
            let off = offset as usize;
            let slice = if off >= data.len() {
                &data[0..0]
            } else {
                let end = (off + size as usize).min(data.len());
                &data[off..end]
            };
            return Ok(slice.to_vec());
        }

        let inode = self
            .core
            .get_inode(ino)
            .map_err(|_| e(posix::ENOENT))?;
        let eff_size = self.effective_size(&inode);
        let start = offset;
        if start >= eff_size {
            return Ok(Vec::new());
        }
        let len = (size as u64).min(eff_size - start) as usize;

        // Block range this read touches.
        let first_block = start / BLOCK_SIZE as u64;
        let last_block = (start + len as u64 - 1) / BLOCK_SIZE as u64;
        let expected_blocks = (last_block - first_block + 1) as usize;
        let cached = self.core.page_cache.get_pages_range(ino, first_block, last_block);

        if cached.len() == expected_blocks {
            // Fast path: entirely page-cache hits. No staging lock, no
            // disk -- this is the concurrent-read scale-out path.
            let mut out = vec![0u8; len];
            let _ = Self::overlay_pages(&mut out, start, &cached);
            return Ok(out);
        }

        // Misses: read the committed/staged state for the whole range
        // through the standard path (which honors the in-flight
        // transaction overlay, checksums, and cipher decode), then
        // overlay the cached pages on top -- cached pages are strictly
        // newer than anything staged or committed.
        let committed = self.with_read_ctx(|ctx, sb| {
            let cctx = self.core.resolve_block_cipher_ctx(ctx, &inode)?;
            crate::file::writer::FileManager::read_file(
                ctx,
                sb.checksum_tree_root,
                sb.bad_blocks_root,
                &cctx,
                &mut inode.clone(),
                start,
                len as u64,
            )
        });
        let mut out = vec![0u8; len];
        match committed {
            Ok(data) => {
                let n = data.len().min(len);
                out[..n].copy_from_slice(&data[..n]);
            }
            // Phase 12 (honesty fix): a committed-read failure is
            // CORRUPTION or IO error -- the 3.3-3.5 path swallowed it
            // into silent zeros (`.ok()`), serving garbage to the
            // application with a success code. Surface EIO unless the
            // page cache covers the whole range (buffered bytes are
            // strictly newer and were never on disk).
            Err(err) => {
                if cached.len() < expected_blocks {
                    return Err(VfsError::from_io(&err));
                }
            }
        }
        let _ = Self::overlay_pages(&mut out, start, &cached);
        Ok(out)
    }

    fn write(&self, ino: u64, offset: u64, data: &[u8]) -> VfsResult<u32> {
        if ino == SCRUB_INO {
            if let Ok(cmd) = std::str::from_utf8(data) {
                self.scrubber.handle_command(cmd.trim());
            }
            return Ok(data.len() as u32);
        }
        if ino == HEALTH_INO {
            return Err(e(posix::EPERM));
        }

        let now = now_secs();
        let inode = match self.core.get_inode(ino) {
            Ok(inode) => inode,
            Err(_) => return Err(e(posix::EIO)),
        };

        // Compressed inodes (stateful cluster path) and write-through
        // mode keep 3.3's synchronous staging semantics.
        let cacheable = self.core.page_cache.is_enabled() && inode.compression_algo == 0;
        if !cacheable {
            return self.write_through(ino, offset, data, now);
        }

        // Phase 10 parallel intake: per-inode gate, page-level RMW
        // with read-through fetch, shadow size/mtime, then return.
        // The caller's bytes are visible to reads immediately; they
        // become durable at the next flush/commit point.
        
        let gate = self.core.page_cache.gate(ino);
        let _gate_guard = gate.lock().unwrap();

        // Pre-fetch the committed content of at most two boundary blocks
        // (head and tail) that are partial and within committed size.
        // All interior blocks are full 4 KiB writes and require zero fetch.
        // Eliminates heap allocation of HashMap on every write call.
        let committed_size = inode.size;
        let mut head_fetch: Option<(u64, [u8; BLOCK_SIZE])> = None;
        let mut tail_fetch: Option<(u64, [u8; BLOCK_SIZE])> = None;

        if !data.is_empty() {
            let head_block = offset / BLOCK_SIZE as u64;
            let head_within = (offset % BLOCK_SIZE as u64) as usize;
            if head_within != 0
                && (head_block * BLOCK_SIZE as u64) < committed_size
                && !self.core.page_cache.has_page(ino, head_block)
            {
                head_fetch = Some((head_block, self.fetch_block(&inode, head_block)));
            }

            let end = offset + data.len() as u64;
            let tail_within = (end % BLOCK_SIZE as u64) as usize;
            let tail_block = (end - 1) / BLOCK_SIZE as u64;
            if tail_within != 0
                && tail_block != head_block
                && (tail_block * BLOCK_SIZE as u64) < committed_size
                && !self.core.page_cache.has_page(ino, tail_block)
            {
                tail_fetch = Some((tail_block, self.fetch_block(&inode, tail_block)));
            }
        }

        self.core.page_cache.store(ino, offset, data, now, |block| {
            if let Some((b, page)) = head_fetch {
                if b == block {
                    return page;
                }
            }
            if let Some((b, page)) = tail_fetch {
                if b == block {
                    return page;
                }
            }
            [0u8; BLOCK_SIZE]
        });

        // Bounded memory / ZFS-style ARC dirty coalescing:
        // Background flusher drains dirty inodes asynchronously.
        // If hard limit is breached, the writer directly flushes its held inode.
        if self.core.page_cache.total_dirty() > crate::fs::page_cache::FLUSH_HARD_LIMIT_BYTES {
            let _ = self.flush_ino_locked(ino);
        }
        Ok(data.len() as u32)
    }

    fn create(&self, parent: u64, name: &str, create: &VfsCreate) -> VfsResult<VfsAttr> {
        let now = now_secs();

        // Resolve compression/encryption defaults and, if encryption is
        // on by default, generate a fresh key up front -- before any
        // TxContext borrow is in play.
        let sb0 = self.core.sb();
        let compression_algo = sb0.default_compression;
        let encryption_algo = sb0.default_encryption;
        let key_tree_root = sb0.key_tree_root;
        let new_key = if encryption_algo != 0 {
            self.core.key_manager.lock().unwrap().generate_key(encryption_algo).ok()
        } else {
            None
        };
        let key_id = new_key.map(|(id, _)| id).unwrap_or(0);

        let mut final_inode = None;

        {
            let (r, _dirty) = self.with_stage_ctx(now, |ctx, sb| {
                let bg_desc = self.core.get_bg_desc();
                let blocks_per_group = sb.blocks_per_group;
                if let Ok(mut parent_inode) = crate::inode::manager::InodeManager::read_inode(
                    ctx,
                    sb.inode_tree_root,
                    parent,
                ) {
                    // The inode allocator mutates the superblock (free
                    // list); it runs under the staging lock + sb write
                    // lock -- the same mutual exclusion `&mut self`
                    // gave it in 3.3.
                    let new_ino = {
                        let mut sb_mut = self.core.sb_write();
                        crate::inode::manager::InodeManager::allocate_inode(&mut sb_mut)
                    };
                    if let Ok(new_ino) = new_ino {
                        let new_inode = Inode {
                            ino: new_ino,
                            mode: create.mode | posix::S_IFREG,
                            uid: create.uid,
                            gid: create.gid,
                            links_count: 1,
                            flags: 0,
                            padding1: 0,
                            size: 0,
                            ctime: now as i64,
                            mtime: now as i64,
                            atime: now as i64,
                            extent_count: 0,
                            compression_algo,
                            encryption_algo,
                            key_id,
                            extents: [crate::ondisk::serialization::Extent {
                                logical_start: 0,
                                physical_start: 0,
                                length: 0,
                            }; 7],
                            checksum: 0,
                            spill_pad_head: [0; 4],
                            spill_extent_root: 0,
                        };

                        if crate::inode::manager::InodeManager::write_inode_with_allocator(
                            ctx,
                            sb.inode_tree_root,
                            &new_inode,
                            |c| {
                                crate::allocator::bitmap::Allocator::allocate_extents(
                                    c,
                                    &bg_desc,
                                    blocks_per_group,
                                    1,
                                )
                            },
                        )
                        .is_ok()
                        {
                            if key_id != 0 {
                                // Persist the freshly generated key so it's
                                // still there after a remount, not just for
                                // this mount's in-memory cache.
                                let _ = self
                                    .core
                                    .key_manager
                                    .lock()
                                    .unwrap()
                                    .persist(ctx, key_tree_root, key_id, |c| {
                                        crate::allocator::bitmap::Allocator::allocate_extents(
                                            c,
                                            &bg_desc,
                                            blocks_per_group,
                                            1,
                                        )
                                    });
                            }
                            if crate::directory::entries::DirManager::add_entry(
                                ctx,
                                &bg_desc,
                                sb.blocks_per_group,
                                sb.checksum_tree_root,
                                sb.bad_blocks_root,
                                &mut parent_inode,
                                name,
                                new_ino,
                                posix::dirent_type(create.mode | posix::S_IFREG),
                            )
                            .is_ok()
                            {
                                parent_inode.mtime = now as i64;
                                let _ =
                                    crate::inode::manager::InodeManager::write_inode_with_allocator(
                                        ctx,
                                        sb.inode_tree_root,
                                        &parent_inode,
                                        |c| {
                                            crate::allocator::bitmap::Allocator::allocate_extents(
                                                c,
                                                &bg_desc,
                                                blocks_per_group,
                                                1,
                                            )
                                        },
                                    );
                                final_inode = Some(new_inode);
                            }
                        }
                    }
                }
            });
            let (_r, _dirty) = (r, _dirty);
        }

        match final_inode {
            Some(inode) => {
                self.core.dentry_cache.write().unwrap().insert((parent, name.to_string()), inode.ino);
                self.core.inode_cache.insert(inode.ino, inode, false);
                self.core.commit_wake.notify_one();
                Ok(self.to_vfs_attr(&inode))
            }
            None => Err(e(posix::EIO)),
        }
    }

    fn mkdir(&self, parent: u64, name: &str, create: &VfsCreate) -> VfsResult<VfsAttr> {
        let now = now_secs();
        let mut final_inode = None;

        {
            let (r, _dirty) = self.with_stage_ctx(now, |ctx, sb| {
                let bg_desc = self.core.get_bg_desc();
                if let Ok(mut parent_inode) = crate::inode::manager::InodeManager::read_inode(
                    ctx,
                    sb.inode_tree_root,
                    parent,
                ) {
                    let new_ino = {
                        let mut sb_mut = self.core.sb_write();
                        crate::inode::manager::InodeManager::allocate_inode(&mut sb_mut)
                    };
                    if let Ok(new_ino) = new_ino {
                        let mut new_inode = Inode {
                            ino: new_ino,
                            mode: create.mode | posix::S_IFDIR,
                            uid: create.uid,
                            gid: create.gid,
                            links_count: 2,
                            flags: 0,
                            padding1: 0,
                            size: 0,
                            ctime: now as i64,
                            mtime: now as i64,
                            atime: now as i64,
                            extent_count: 0,
                            compression_algo: 0,
                            encryption_algo: 0,
                            key_id: 0,
                            extents: [crate::ondisk::serialization::Extent {
                                logical_start: 0,
                                physical_start: 0,
                                length: 0,
                            }; 7],
                            checksum: 0,
                            spill_pad_head: [0; 4],
                            spill_extent_root: 0,
                        };

                        let blocks_per_group = sb.blocks_per_group;

                        // 3.6 (ACLs): default-ACL inheritance on
                        // mkdir -- the child's ACCESS ACL is the
                        // parent's DEFAULT ACL intersected with the
                        // create mode, and the child (a directory)
                        // also receives a copy of the DEFAULT ACL.
                        // Both land in the SAME transaction as the
                        // directory's inode write below.
                        if let Some(default_acl) = self.read_acl_in_ctx(
                            ctx,
                            sb,
                            parent,
                            crate::security::posix_acl::ACL_DEFAULT_XATTR,
                        ) {
                            let access_acl =
                                default_acl.inherit_access_from_default(create.mode);
                            let bg2 = self.core.get_bg_desc();
                            let mut allocate2 = |c: &mut TxContext| {
                                crate::allocator::bitmap::Allocator::allocate_extents_meta(
                                    c,
                                    &bg2,
                                    blocks_per_group,
                                    1,
                                )
                            };
                            if let Ok((root, to_free)) = crate::fs::xattrs::set_xattr(
                                ctx,
                                sb.xattr_tree_root,
                                new_ino,
                                crate::security::posix_acl::ACL_ACCESS_XATTR,
                                &access_acl.encode(),
                                0,
                                false,
                                &mut allocate2,
                            ) {
                                for b in to_free {
                                    let _ = crate::allocator::bitmap::Allocator::free_extents(
                                        ctx, &bg2, b, 1,
                                    );
                                }
                                if root != sb.xattr_tree_root {
                                    self.core.sb_write().xattr_tree_root = root;
                                }
                                self.core.sb_write().fs_features |=
                                    crate::common::version::FS_FEATURE_XATTR;
                                let _ = crate::fs::xattrs::set_xattr(
                                    ctx,
                                    sb.xattr_tree_root,
                                    new_ino,
                                    crate::security::posix_acl::ACL_DEFAULT_XATTR,
                                    &default_acl.encode(),
                                    0,
                                    false,
                                    &mut allocate2,
                                );
                            }
                            // The mode bits become ACL-derived.
                            new_inode.mode =
                                (new_inode.mode & !0o777) | access_acl.mode_bits();
                        }

                        if crate::inode::manager::InodeManager::write_inode_with_allocator(
                            ctx,
                            sb.inode_tree_root,
                            &new_inode,
                            |c| {
                                crate::allocator::bitmap::Allocator::allocate_extents(
                                    c,
                                    &bg_desc,
                                    blocks_per_group,
                                    1,
                                )
                            },
                        )
                        .is_ok()
                            && crate::directory::entries::DirManager::add_entry(
                                ctx,
                                &bg_desc,
                                sb.blocks_per_group,
                                sb.checksum_tree_root,
                                sb.bad_blocks_root,
                                &mut parent_inode,
                                name,
                                new_ino,
                                posix::dirent_type(create.mode | posix::S_IFDIR),
                            )
                            .is_ok()
                        {
                            parent_inode.mtime = now as i64;
                            let _ =
                                crate::inode::manager::InodeManager::write_inode_with_allocator(
                                    ctx,
                                    sb.inode_tree_root,
                                    &parent_inode,
                                    |c| {
                                        crate::allocator::bitmap::Allocator::allocate_extents(
                                            c,
                                            &bg_desc,
                                            blocks_per_group,
                                            1,
                                        )
                                    },
                                );

                            // Also add . and .. to the new directory.
                            let _ = crate::directory::entries::DirManager::add_entry(
                                ctx,
                                &bg_desc,
                                sb.blocks_per_group,
                                sb.checksum_tree_root,
                                sb.bad_blocks_root,
                                &mut new_inode,
                                ".",
                                new_ino,
                                2,
                            );
                            let _ = crate::directory::entries::DirManager::add_entry(
                                ctx,
                                &bg_desc,
                                sb.blocks_per_group,
                                sb.checksum_tree_root,
                                sb.bad_blocks_root,
                                &mut new_inode,
                                "..",
                                parent,
                                2,
                            );

                            final_inode = Some(new_inode);
                        }
                    }
                }
            });
            let (_r, _dirty) = (r, _dirty);
        }

        match final_inode {
            Some(inode) => {
                self.core.dentry_cache.write().unwrap().insert((parent, name.to_string()), inode.ino);
                self.core.inode_cache.insert(inode.ino, inode, false);
                self.core.commit_wake.notify_one();
                Ok(self.to_vfs_attr(&inode))
            }
            None => Err(e(posix::EIO)),
        }
    }

    fn unlink(&self, parent: u64, name: &str) -> VfsResult<()> {
        let now = now_secs();
        let mut target_ino_out: Option<u64> = None;

        {
            let (r, _dirty) = self.with_stage_ctx(now, |ctx, sb| {
                let bg_desc = self.core.get_bg_desc();
                let blocks_per_group = sb.blocks_per_group;
                if let Ok(mut parent_inode) = crate::inode::manager::InodeManager::read_inode(
                    ctx,
                    sb.inode_tree_root,
                    parent,
                ) {
                    if let Ok(Some(target_ino)) =
                        crate::directory::entries::DirManager::remove_entry(
                            ctx,
                            &bg_desc,
                            sb.blocks_per_group,
                            sb.checksum_tree_root,
                            sb.bad_blocks_root,
                            &mut parent_inode,
                            name,
                        )
                    {
                        if let Ok(mut target_inode) =
                            crate::inode::manager::InodeManager::read_inode(
                                ctx,
                                sb.inode_tree_root,
                                target_ino,
                            )
                        {
                            target_inode.links_count -= 1;
                            if target_inode.links_count == 0 {
                                target_inode.mode = 0; // free inode
                            }
                            let _ =
                                crate::inode::manager::InodeManager::write_inode_with_allocator(
                                    ctx,
                                    sb.inode_tree_root,
                                    &target_inode,
                                    |c| {
                                        crate::allocator::bitmap::Allocator::allocate_extents(
                                            c,
                                            &bg_desc,
                                            blocks_per_group,
                                            1,
                                        )
                                    },
                                );

                            parent_inode.mtime = now as i64;
                            let _ =
                                crate::inode::manager::InodeManager::write_inode_with_allocator(
                                    ctx,
                                    sb.inode_tree_root,
                                    &parent_inode,
                                    |c| {
                                        crate::allocator::bitmap::Allocator::allocate_extents(
                                            c,
                                            &bg_desc,
                                            blocks_per_group,
                                            1,
                                        )
                                    },
                                );
                            target_ino_out = Some(target_ino);
                        }
                    }
                }
            });
            let (_r, _dirty) = (r, _dirty);
        }

        match target_ino_out {
            Some(target_ino) => {
                self.core.dentry_cache.write().unwrap().remove(&(parent, name.to_string()));
                self.core.commit_wake.notify_one();
                // Phase 10: an unlinked file's unsynced buffered pages
                // were never promised durability (the standard
                // write-back contract) -- drop them with the link.
                self.core.page_cache.drop_ino(target_ino);
                self.core.inode_cache.invalidate(target_ino);
                Ok(())
            }
            None => Err(e(posix::ENOENT)),
        }
    }

    fn rmdir(&self, parent: u64, name: &str) -> VfsResult<()> {
        // Directory removal goes through unlink semantics plus a
        // not-empty guard: look up the target first.
        let target = self.with_read_ctx(|ctx, sb| {
            let mut parent_inode = match crate::inode::manager::InodeManager::read_inode(
                ctx,
                sb.inode_tree_root,
                parent,
            ) {
                Ok(p) => p,
                Err(_) => return Err(e(posix::ENOENT)),
            };
            match crate::directory::entries::DirManager::read_entries(
                ctx,
                sb.checksum_tree_root,
                sb.bad_blocks_root,
                &mut parent_inode,
            ) {
                Ok(entries) => match entries.iter().find(|en| en.name == name) {
                    Some(entry) => {
                        if entry.file_type != 2 {
                            return Err(e(posix::ENOTDIR));
                        }
                        // Not-empty check: entries beyond . and ..
                        // (read_entries returns only the real ones).
                        if !entries.is_empty() {
                            return Err(e(posix::ENOTEMPTY));
                        }
                        Ok(())
                    }
                    None => Err(e(posix::ENOENT)),
                },
                Err(_) => Err(e(posix::EIO)),
            }
        });
        target?;
        // Empty: fall through to the unlink path (same 1.x semantics).
        self.unlink(parent, name)
    }

    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> VfsResult<()> {
        let now = now_secs();
        let mut success = false;

        {
            let (r, _dirty) = self.with_stage_ctx(now, |ctx, sb| {
                let bg_desc = self.core.get_bg_desc();
                let blocks_per_group = sb.blocks_per_group;
                let checksum_tree_root = sb.checksum_tree_root;
                let bad_blocks_root = sb.bad_blocks_root;
                let inode_tree_root = sb.inode_tree_root;
                if let Ok(mut p_inode) =
                    crate::inode::manager::InodeManager::read_inode(ctx, inode_tree_root, parent)
                {
                    if let Ok(Some(target_ino)) =
                        crate::directory::entries::DirManager::remove_entry(
                            ctx,
                            &bg_desc,
                            blocks_per_group,
                            checksum_tree_root,
                            bad_blocks_root,
                            &mut p_inode,
                            name,
                        )
                    {
                        if let Ok(target_inode) =
                            crate::inode::manager::InodeManager::read_inode(
                                ctx,
                                inode_tree_root,
                                target_ino,
                            )
                        {
                            let file_type = posix::dirent_type(target_inode.mode);

                            // Same-directory rename reuses p_inode for both
                            // sides; cross-directory rename loads the
                            // destination parent separately and updates both.
                            let add_result = if parent == newparent {
                                crate::directory::entries::DirManager::add_entry(
                                    ctx,
                                    &bg_desc,
                                    blocks_per_group,
                                    checksum_tree_root,
                                    bad_blocks_root,
                                    &mut p_inode,
                                    newname,
                                    target_ino,
                                    file_type,
                                )
                            } else if let Ok(mut np_inode) =
                                crate::inode::manager::InodeManager::read_inode(
                                    ctx,
                                    inode_tree_root,
                                    newparent,
                                )
                            {
                                let r = crate::directory::entries::DirManager::add_entry(
                                    ctx,
                                    &bg_desc,
                                    blocks_per_group,
                                    checksum_tree_root,
                                    bad_blocks_root,
                                    &mut np_inode,
                                    newname,
                                    target_ino,
                                    file_type,
                                );
                                if r.is_ok() {
                                    np_inode.mtime = now as i64;
                                    let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                                        ctx,
                                        inode_tree_root,
                                        &np_inode,
                                        |c| {
                                            crate::allocator::bitmap::Allocator::allocate_extents(
                                                c,
                                                &bg_desc,
                                                blocks_per_group,
                                                1,
                                            )
                                        },
                                    );
                                }
                                r
                            } else {
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    "destination parent inode not found",
                                ))
                            };

                            if add_result.is_ok() {
                                p_inode.mtime = now as i64;
                                let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                                    ctx,
                                    inode_tree_root,
                                    &p_inode,
                                    |c| {
                                        crate::allocator::bitmap::Allocator::allocate_extents(
                                            c,
                                            &bg_desc,
                                            blocks_per_group,
                                            1,
                                        )
                                    },
                                );
                                success = true;
                            } else {
                                // The entry was already removed from the source
                                // directory; since it couldn't be re-added at
                                // the destination, put it back rather than
                                // leaving the inode orphaned.
                                let _ = crate::directory::entries::DirManager::add_entry(
                                    ctx,
                                    &bg_desc,
                                    blocks_per_group,
                                    checksum_tree_root,
                                    bad_blocks_root,
                                    &mut p_inode,
                                    name,
                                    target_ino,
                                    file_type,
                                );
                            }
                        }
                    }
                }
            });
            let (_r, _dirty) = (r, _dirty);
        }

        if success {
            {
                let mut dcache = self.core.dentry_cache.write().unwrap();
                dcache.remove(&(parent, name.to_string()));
                dcache.remove(&(newparent, newname.to_string()));
            }
            self.core.commit_wake.notify_one();
            Ok(())
        } else {
            Err(e(posix::ENOENT))
        }
    }

    fn fsync(&self, ino: u64, _datasync: bool) -> VfsResult<()> {
        // Phase 10: fsync is the durability barrier -- flush this
        // inode's buffered pages through staging, then drive commits
        // until one covers our staging (coalesced with any concurrent
        // fsyncs: one journal run + one set of syncs serves the group),
        // then sync the device.
        let txids = self
            .flush_ino_marked(ino)
            .map_err(|_| e(posix::EIO))?;
        self.commit_until_covered(&txids);
        // No caller-level device sync: covering the transaction means
        // its journal + syncs + apply + root cells all landed
        // (`commit_tx`'s own post-apply `disk.sync()` IS the WAL
        // durability point), and a clean fsync (nothing staged,
        // nothing committed) has nothing on the device that the
        // vfs path left unsynced -- all disk writes flow through
        // `commit_tx`'s apply.
        Ok(())
    }

    fn flush(&self, ino: u64) -> VfsResult<()> {
        // FUSE close(): flush the pages and drive the transaction they
        // landed in to retirement (no extra device sync -- 3.3
        // semantics), sharing any concurrent commit.
        let txids = self
            .flush_ino_marked(ino)
            .map_err(|_| e(posix::EIO))?;
        self.commit_until_covered(&txids);
        Ok(())
    }

    fn statfs(&self, _ino: u64) -> VfsResult<VfsStatFs> {
        let sb = self.core.sb();
        let stats = crate::fs::stat::compute_stats(&sb);
        Ok(VfsStatFs {
            total_blocks: stats.total_blocks,
            free_blocks: stats.free_blocks,
            avail_blocks: stats.free_blocks,
            total_inodes: stats.total_inodes,
            free_inodes: stats.free_inodes_estimate,
            block_size: stats.block_size,
            max_name_len: stats.max_name_len,
        })
    }

    fn access(&self, ino: u64, uid: u32, gid: u32, mask: i32) -> VfsResult<()> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Ok(());
        }
        let inode = self.core.get_inode(ino).map_err(|_| e(posix::ENOENT))?;
        // 3.6: an ACL-bearing inode is judged by its ACL (draft 17);
        // everything else keeps the mode-bit check.
        if let Some(acl) = self.read_acl(ino, crate::security::posix_acl::ACL_ACCESS_XATTR) {
            let want = mask_to_perm(mask);
            if acl.evaluate(uid, gid, inode.uid, inode.gid, want) {
                Ok(())
            } else {
                Err(e(posix::EACCES))
            }
        } else if crate::inode::permissions::check_access(&inode, uid, gid, mask) {
            Ok(())
        } else {
            Err(e(posix::EACCES))
        }
    }

    fn readlink(&self, _ino: u64) -> VfsResult<String> {
        // Symlinks are not yet first-class in the on-disk format (the
        // 1.x status documented this); the bridge surfaces ENOSYS.
        Err(VfsError::nosys())
    }

    fn symlink(
        &self,
        _parent: u64,
        _name: &str,
        _target: &str,
        _uid: u32,
        _gid: u32,
    ) -> VfsResult<VfsAttr> {
        Err(VfsError::nosys())
    }

    // -- 3.6: extended attributes, POSIX ACLs, reflink -------------------

    fn getxattr(&self, ino: u64, name: &str) -> VfsResult<Option<Vec<u8>>> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Ok(None);
        }
        let root = self.core.sb().xattr_tree_root;
        self.with_read_ctx(|ctx, sb| {
            crate::fs::xattrs::get_xattr(ctx, sb.xattr_tree_root, ino, name)
        })
        .map_err(|err| VfsError::from_io(&err))
        .or_else(|_| {
            // Fall back to the caller-supplied root snapshot if the
            // scratch context raced a tree move.
            if root == 0 {
                Ok(None)
            } else {
                Err(VfsError::io())
            }
        })
    }

    fn setxattr(&self, ino: u64, name: &str, value: &[u8], flags: i32) -> VfsResult<()> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Err(e(posix::EPERM));
        }
        let inode = self.core.get_inode(ino).map_err(|_| e(posix::ENOENT))?;
        let is_symlink = posix::is_lnk(inode.mode);
        let now = now_secs();
        let is_acl_access = name == crate::security::posix_acl::ACL_ACCESS_XATTR;
        let is_acl_default = name == crate::security::posix_acl::ACL_DEFAULT_XATTR;

        // ACL xattrs validate BEFORE anything is staged.
        let acl = if is_acl_access || is_acl_default {
            Some(
                crate::security::posix_acl::PosixAcl::decode(value)
                    .and_then(|a| {
                        a.validate(is_acl_default)?;
                        Ok(a)
                    })
                    .map_err(|err| VfsError::from_io(&err))?,
            )
        } else {
            None
        };

        let mut result = Ok(());
        let (_r, _dirty) = self.with_stage_ctx(now, |ctx, sb| {
            let bg_desc = self.core.get_bg_desc();
            let blocks_per_group = sb.blocks_per_group;
            let mut allocate = |c: &mut TxContext| {
                crate::allocator::bitmap::Allocator::allocate_extents_meta(
                    c,
                    &bg_desc,
                    blocks_per_group,
                    1,
                )
            };
            // First use: the xattr tree does not exist yet.
            let mut tree_root = sb.xattr_tree_root;
            if tree_root == 0 {
                match (|| {
                    let root = allocate(ctx)?;
                    crate::fs::xattrs::XattrManager::init_empty(ctx, root)?;
                    // First use: publish the new tree's root through
                    // the transaction root cell, the SAME mechanism a
                    // B-tree root move uses -- this is what makes the
                    // pointer durable at this commit (commit_tx writes
                    // the superblock slots when any root cell moved).
                    ctx.set_root_cell(crate::fs::xattrs::XATTR_TREE_NODE_TYPE, root);
                    Ok(root)
                })() {
                    Ok(root) => {
                        tree_root = root;
                        let mut sbw = self.core.sb_write();
                        sbw.xattr_tree_root = root;
                        sbw.fs_features |= crate::common::version::FS_FEATURE_XATTR;
                    }
                    Err(err) => {
                        result = Err(err);
                        return;
                    }
                }
            }
            match crate::fs::xattrs::set_xattr(
                ctx,
                tree_root,
                ino,
                name,
                value,
                flags,
                is_symlink,
                &mut allocate,
            ) {
                Ok((new_root, to_free)) => {
                    for b in to_free {
                        let _ = crate::allocator::bitmap::Allocator::free_extents(
                            ctx, &bg_desc, b, 1,
                        );
                    }
                    if new_root != tree_root {
                        self.core.sb_write().xattr_tree_root = new_root;
                    }
                    // An access-ACL set re-derives the inode's mode
                    // bits (draft 17 §17) in the same transaction.
                    if let Some(a) = &acl {
                        if is_acl_access {
                            if let Ok(mut inode) =
                                crate::inode::manager::InodeManager::read_inode(
                                    ctx,
                                    sb.inode_tree_root,
                                    ino,
                                )
                            {
                                inode.mode =
                                    (inode.mode & !0o777) | a.mode_bits();
                                inode.ctime = now as i64;
                                let _ = crate::inode::manager::InodeManager::
                                    write_inode_with_allocator(
                                        ctx,
                                        sb.inode_tree_root,
                                        &inode,
                                        |c| crate::allocator::bitmap::Allocator::
                                            allocate_extents(
                                                c,
                                                &bg_desc,
                                                blocks_per_group,
                                                1,
                                            ),
                                    );
                                self.core.inode_cache.insert(ino, inode, false);
                            }
                        }
                    }
                    // Every xattr mutation bumps ctime.
                    if !is_acl_access {
                        if let Ok(mut inode) = crate::inode::manager::InodeManager::read_inode(
                            ctx,
                            sb.inode_tree_root,
                            ino,
                        ) {
                            inode.ctime = now as i64;
                            let _ = crate::inode::manager::InodeManager::
                                write_inode_with_allocator(
                                    ctx,
                                    sb.inode_tree_root,
                                    &inode,
                                    |c| crate::allocator::bitmap::Allocator::allocate_extents(
                                        c,
                                        &bg_desc,
                                        blocks_per_group,
                                        1,
                                    ),
                                );
                            self.core.inode_cache.insert(ino, inode, false);
                        }
                    }
                }
                Err(err) => result = Err(err),
            }
        });
        result.map_err(|err| VfsError::from_io(&err))?;
        self.core.commit_one();
        Ok(())
    }

    fn listxattr(&self, ino: u64) -> VfsResult<Vec<String>> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Ok(Vec::new());
        }
        self.with_read_ctx(|ctx, sb| {
            crate::fs::xattrs::list_xattrs(ctx, sb.xattr_tree_root, ino)
        })
        .map_err(|err| VfsError::from_io(&err))
    }

    fn removexattr(&self, ino: u64, name: &str) -> VfsResult<()> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Err(e(posix::EPERM));
        }
        if name == crate::security::posix_acl::ACL_ACCESS_XATTR
            || name == crate::security::posix_acl::ACL_DEFAULT_XATTR
        {
            // Linux: ACL xattrs are removed via chmod/ setxattr with a
            // trivial ACL, never directly.
            return Err(e(posix::EACCES));
        }
        if self.core.get_inode(ino).is_err() {
            return Err(e(posix::ENOENT));
        }
        let now = now_secs();
        let mut result = Ok(());
        let (_r, _dirty) = self.with_stage_ctx(now, |ctx, sb| {
            let bg_desc = self.core.get_bg_desc();
            let blocks_per_group = sb.blocks_per_group;
            if sb.xattr_tree_root == 0 {
                result = Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no xattrs on this volume",
                ));
                return;
            }
            let mut allocate = |c: &mut TxContext| {
                crate::allocator::bitmap::Allocator::allocate_extents_meta(
                    c,
                    &bg_desc,
                    blocks_per_group,
                    1,
                )
            };
            match crate::fs::xattrs::remove_xattr(
                ctx,
                sb.xattr_tree_root,
                ino,
                name,
                &mut allocate,
            ) {
                Ok((existed, new_root, to_free)) => {
                    for b in to_free {
                        let _ = crate::allocator::bitmap::Allocator::free_extents(
                            ctx, &bg_desc, b, 1,
                        );
                    }
                    if new_root != sb.xattr_tree_root {
                        self.core.sb_write().xattr_tree_root = new_root;
                    }
                    if !existed {
                        result = Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "xattr not found",
                        ));
                    }
                }
                Err(err) => result = Err(err),
            }
        });
        result.map_err(|err| VfsError::from_io(&err))?;
        self.core.commit_one();
        Ok(())
    }

    fn copy_file_range(
        &self,
        ino_in: u64,
        offset_in: u64,
        ino_out: u64,
        offset_out: u64,
        len: u64,
    ) -> VfsResult<u64> {
        if ino_in == ino_out {
            return Err(e(posix::EINVAL));
        }
        let src = self.core.get_inode(ino_in).map_err(|_| e(posix::ENOENT))?;
        let dst = self.core.get_inode(ino_out).map_err(|_| e(posix::ENOENT))?;
        let src_eff = self.effective_size(&src);
        let now = now_secs();

        // Whole-file, zero-offset, empty-destination request on a
        // cloneable source: the REFLINK path (shared physical blocks
        // under pinning; O(extents) metadata, zero data copied).
        let whole_file = offset_in == 0
            && offset_out == 0
            && len >= src_eff
            && dst.size == 0
            && !self.core.page_cache.has_pages(ino_out);
        if whole_file {
            match crate::fs::reflink::feasibility(&src) {
                f if f.supported && src_eff > 0 => {
                    let mut result: std::io::Result<u64> = Ok(0);
                    let (_r, _d) = self.with_stage_ctx(now, |ctx, sb| {
                        result = crate::fs::reflink::reflink_file_in_ctx(
                            &self.core,
                            ctx,
                            sb,
                            ino_in,
                            ino_out,
                        );
                    });
                    result.map_err(|err| VfsError::from_io(&err))?;
                    self.core.commit_one();
                    // The destination's cached (empty) inode is now
                    // stale: drop it so reads see the clone's size.
                    self.core.inode_cache.invalidate(ino_out);
                    return Ok(src_eff);
                }
                f if f.supported && src_eff == 0 => {
                    return Ok(0); // cloning an empty file: nothing to share
                }
                _ => {
                    // Compressed source: fall through to the honest byte
                    // copy below (the data is still correct end to end).
                }
            }
        }

        // Byte copy (kernel copy_file_range semantics): read the
        // source range through the standard path, write it to the
        // destination at the requested offset.
        let src_len = src_eff.saturating_sub(offset_in);
        let total = len.min(src_len);
        if total == 0 {
            return Ok(0);
        }
        const CHUNK: u64 = 1024 * 1024;
        let mut copied: u64 = 0;
        while copied < total {
            let n = (total - copied).min(CHUNK);
            let data = self.read(ino_in, offset_in + copied, n as u32)?;
            if data.is_empty() {
                break;
            }
            let written = self.write(ino_out, offset_out + copied, &data)?;
            copied += written as u64;
            if (written as u64) < n {
                break;
            }
        }
        Ok(copied)
    }
}

/// POSIX access(2) masks -> the ACL rwx permission word.
fn mask_to_perm(mask: i32) -> u16 {
    let mut perm = 0u16;
    if mask & 4 != 0 {
        perm |= crate::security::posix_acl::PERM_READ;
    }
    if mask & 2 != 0 {
        perm |= crate::security::posix_acl::PERM_WRITE;
    }
    if mask & 1 != 0 {
        perm |= crate::security::posix_acl::PERM_EXECUTE;
    }
    perm
}

impl LionFS {
    /// Read + decode an ACL xattr through a read context. None = no
    /// ACL stored (or undecodable -- the mode-bit fallback then
    /// applies, the conservative direction).
    fn read_acl(&self, ino: u64, name: &str) -> Option<crate::security::posix_acl::PosixAcl> {
        if self.core.sb().xattr_tree_root == 0 {
            return None;
        }
        let bytes = self
            .with_read_ctx(|ctx, sb| {
                crate::fs::xattrs::get_xattr(ctx, sb.xattr_tree_root, ino, name)
            })
            .ok()??;
        crate::security::posix_acl::PosixAcl::decode(&bytes).ok()
    }

    /// Context-based ACL read for callers already inside a staging
    /// closure (setattr chmod sync, mkdir inheritance).
    fn read_acl_in_ctx(
        &self,
        ctx: &mut TxContext,
        sb: &crate::ondisk::serialization::Superblock,
        ino: u64,
        name: &str,
    ) -> Option<crate::security::posix_acl::PosixAcl> {
        if sb.xattr_tree_root == 0 {
            return None;
        }
        let bytes =
            crate::fs::xattrs::get_xattr(ctx, sb.xattr_tree_root, ino, name).ok()??;
        crate::security::posix_acl::PosixAcl::decode(&bytes).ok()
    }

    fn virtual_attr(ino: u64) -> VfsAttr {
        VfsAttr {
            ino,
            size: 0,
            blocks: 0,
            kind: VfsKind::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: 0,
            gid: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            blksize: BLOCK_SIZE as u32,
            flags: 0,
        }
    }

    /// Overlays cached (newest) pages onto `out`, which starts at file
    /// offset `start`. Returns the number of bytes overlaid.
    fn overlay_pages(out: &mut [u8], start: u64, cached: &[(u64, [u8; BLOCK_SIZE])]) -> usize {
        let mut overlaid = 0usize;
        for &(block, page) in cached {
            let block_start = block * BLOCK_SIZE as u64;
            let block_end = block_start + BLOCK_SIZE as u64;
            if block_end <= start || block_start >= start + out.len() as u64 {
                continue;
            }
            let from = (start.max(block_start) - block_start) as usize;
            let to = ((start + out.len() as u64).min(block_end) - block_start) as usize;
            let out_from = (block_start.max(start) - start) as usize;
            let n = to - from;
            out[out_from..out_from + n].copy_from_slice(&page[from..to]);
            overlaid += n;
        }
        overlaid
    }

    /// 3.3's synchronous write path, byte for byte, for compressed
    /// inodes and `LFS_WRITEBACK=0` (write-through mode). Stages the
    /// write into the shared transaction under the staging lock and
    /// commits when the transaction crosses 8192 dirty blocks (~32 MiB).
    fn write_through(&self, ino: u64, offset: u64, data: &[u8], now: u64) -> VfsResult<u32> {
        let mut success = false;
        let mut dirty_after = 0usize;

        {
            let (r, dirty) = self.with_stage_ctx(now, |ctx, sb| {
                let bg_desc = self.core.get_bg_desc();
                let blocks_per_group = sb.blocks_per_group;
                let inode_tree_root = sb.inode_tree_root;
                let key_tree_root = sb.key_tree_root;
                let crypto_tree_root = sb.crypto_tree_root;
                if let Ok(mut inode) =
                    crate::inode::manager::InodeManager::read_inode(ctx, inode_tree_root, ino)
                {
                    // Resolve the cipher context: fresh key material for
                    // encrypted files (the 1.x resolve_block_cipher_ctx
                    // logic, inlined for the borrow structure).
                    let key = if inode.encryption_algo != 0 {
                        self.core
                            .key_manager
                            .lock()
                            .unwrap()
                            .get_key(ctx, key_tree_root, inode.key_id)
                            .ok()
                            .flatten()
                    } else {
                        None
                    };
                    let cctx = crate::security::block_cipher::BlockCipherContext {
                        compression_algo: inode.compression_algo,
                        encryption_algo: inode.encryption_algo,
                        key,
                        crypto_tree_root,
                    };
                    if crate::file::writer::FileManager::write_file(
                        ctx,
                        &bg_desc,
                        blocks_per_group,
                        sb.checksum_tree_root,
                        sb.refcount_tree_root,
                        sb.dedupe_tree_root,
                        &cctx,
                        &mut inode,
                        offset,
                        data,
                    )
                    .is_ok()
                    {
                        inode.mtime = now as i64;
                        // Use the real allocator (not the guaranteed-to-error
                        // dummy) so this doesn't start silently failing once
                        // the inode tree grows past its first leaf node.
                        let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                            ctx,
                            inode_tree_root,
                            &inode,
                            |c| {
                                crate::allocator::bitmap::Allocator::allocate_extents(
                                    c,
                                    &bg_desc,
                                    blocks_per_group,
                                    1,
                                )
                            },
                        );
                        self.core.inode_cache.insert(ino, inode, false);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            });
            success = r;
            dirty_after = dirty;
        }

        if success {
            if dirty_after > 8192 {
                self.core.commit_one();
            }
            Ok(data.len() as u32)
        } else {
            Err(e(posix::EIO))
        }
    }
}

fn secs_of(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vfs_error_mapping_is_stable() {
        assert_eq!(e(posix::ENOENT).errno, 2);
        assert_eq!(e(posix::EIO).errno, 5);
    }

    #[test]
    fn virtual_inodes_are_stable_constants() {
        // The control-file inodes must never change: they are visible in
        // mounted filesystems.
        assert_eq!(SCRUB_INO, 999_999);
        assert_eq!(HEALTH_INO, 999_998);
    }

    #[test]
    fn overlay_pages_slices_correctly() {
        // One cached page (block 1), read starting mid-block.
        let mut page = [0u8; BLOCK_SIZE];
        for (i, b) in page.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let out_len = 100;
        let start = BLOCK_SIZE as u64 + 17;
        let mut out = vec![0xFFu8; out_len];
        let n = LionFS::overlay_pages(&mut out, start, &[(1, page)]);
        assert_eq!(n, out_len);
        assert_eq!(&out[..], &page[17..117]);
        // Read wholly inside one cached page.
        let mut out2 = vec![0u8; 10];
        let n2 = LionFS::overlay_pages(&mut out2, 3, &[(0, page)]);
        assert_eq!(n2, 10);
        assert_eq!(&out2[..], &page[3..13]);
    }
}
