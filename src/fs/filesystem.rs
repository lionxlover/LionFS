#![allow(
    clippy::collapsible_if,
    clippy::manual_div_ceil,
    clippy::unnecessary_cast
)]

//! The portable core of the mounted filesystem: disk, superblock,
//! transaction manager, caches, and mount-time recovery. The operations
//! surface lives in [`super::vfs_impl`] (platform-neutral `VfsOps`);
//! platform bridges live in [`crate::vfs`]. This file contains no
//! fuser/libc code, so the core compiles on every platform the PAL
//! supports.
//!
//! ## Phase 10: the `SharedCore` split
//!
//! 3.3's `LionFS` was single-threaded by construction -- `VfsOps`
//! methods took `&mut self`, so every bridge serialized on the whole
//! struct. 3.4 splits the mount into two pieces:
//!
//! * [`SharedCore`] (behind an `Arc`): every piece of shared mutable
//!   state, each behind its own lock -- the write-back page cache
//!   (per-inode gates), the staging transaction (`active_tx` Mutex:
//!   the single-writer journal/staging lock, unchanged from 3.3 in
//!   WHAT it protects), the superblock (`RwLock` with `Copy`
//!   snapshots for lock-cheap reads), the key manager, and the inode
//!   cache (moka `sync`, already concurrent). The disk itself was
//!   already `&self`-thread-safe since 3.3's smpbench proved it with
//!   N-thread reads.
//! * [`LionFS`]: the mount-lifecycle shell (scrubber worker, image
//!   path) plus the `Arc<SharedCore>` every method shares.
//!
//! The lock hierarchy, to keep deadlocks impossible:
//! **per-inode gate -> (active_tx Mutex -> superblock write-lock |
//! key_manager Mutex)**. Readers of committed state never take the
//! staging lock unless a transaction is actually in flight (they then
//! read through its overlay, exactly the 3.3 `with_ctx` semantics).

use crate::disk::block_io::Disk;
use crate::fs::page_cache::PageCache;
use crate::ondisk::serialization::{Inode, Superblock, BLOCK_SIZE};
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::{Transaction, TxContext};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, RwLock};
use std::time::Duration;

use crate::cache::inode_cache::InodeCache;
use crate::security::block_cipher::BlockCipherContext;
use crate::security::keys::KeyManager;

/// Phase 11 committer coordination state. `shutdown` is set by
/// `stop_committer` (mount destroy); the wake/done condvars live on
/// `SharedCore` and share this mutex.
#[derive(Default)]
pub struct CommitCoord {
    pub shutdown: bool,
}

/// Lock a mutex without panicking on poisoning. A panicked committer
/// is a bug we surface loudly elsewhere; cascading a second panic into
/// every VFS caller turns one fault into an unkillable mount. The data
/// behind these locks (coordination flags, the pending list) is
/// either recoverable or the mount is already in trouble -- taking
/// the guard is strictly better than unwinding.
macro_rules! lock_ok {
    ($m:expr) => {
        $m.lock().unwrap_or_else(|p| p.into_inner())
    };
}

/// Everything a `VfsOps` call touches, safe to share across threads.
/// See the module docs for the locking discipline.
pub struct SharedCore {
    pub disk: Arc<Disk>,
    superblock: RwLock<Superblock>,
    pub tx_manager: TransactionManager,
    /// THE staging lock (Phase 10): writers hold this while staging
    /// metadata through a `Transaction`, exactly the region 3.3's
    /// `&mut self` implicitly serialized. Readers take it only to read
    /// through an in-flight transaction's overlay.
    pub active_tx: Mutex<Option<Transaction>>,
    /// Phase 11 (pipelined txg): transactions QUIESCED out of
    /// `active_tx` whose journal / apply / syncs / root-cell switch
    /// have not all landed. Oldest first. Readers and later stagers
    /// consult these overlays (`TxContext::read_block`) so a group in
    /// flight to disk stays readable and buildable-on -- see the
    /// Phase 11 design record (`specifications/phase11_txg_birth.md`).
    pub committing: Mutex<Vec<Arc<Transaction>>>,
    /// Phase 11: fast probe for "a staging transaction exists" (set
    /// under the staging lock at begin, cleared at quiesce). Readers
    /// check this BEFORE taking the staging lock: the no-transaction
    /// case goes straight to the lock-free seqlock + pending-overlay
    /// scratch read, so N readers never ping one Mutex. A racing
    /// reader may see committed state while a writer stages --
    /// stale-but-never-torn, which the POSIX read/write contract
    /// allows (and the page cache's shadow preserves
    /// read-your-own-buffered-write).
    pub tx_present: AtomicBool,
    /// Phase 11: serializes the I/O PHASE of commits, which covers the
    /// journal write, both syncs, the apply loop, and the root-cell
    /// switch. Quiesce does NOT take it: group N+1 accumulates while
    /// group N's I/O runs, but two concurrent appliers could write the
    /// same physical block in the wrong order (the journal's
    /// last-writer-wins is a recovery property, not a live one), so
    /// applies are strictly ordered here.
    pub commit_io: Mutex<()>,
    /// Phase 11: committer-thread wake/sleep coordination (shared by
    /// `commit_wake` and `commit_done` condvars).
    pub commit_coord: Mutex<CommitCoord>,
    /// Phase 11: count of groups in the pending list. Purely a
    /// lock-free fast-path hint: readers skip the `committing` mutex
    /// entirely when it is 0 (the steady read case), because with no
    /// pending group there is no overlay to consult. Correctness does
    /// NOT depend on its freshness: a reader that races a quiesce
    /// sees either a stale 0 (no overlay -- fine, the seqlock
    /// re-validation catches any commit that started mid-read and
    /// retries with the fresh list) or a stale positive (takes the
    /// lock, finds the list already empty -- wasted lock, still
    /// correct). Only mutated inside the odd seqlock window.
    pub pending_count: AtomicU64,
    /// Signaled (with `commit_coord` held) whenever staged bytes may
    /// need committing -- the committer thread's work signal.
    pub commit_wake: Condvar,
    /// Signaled (with `commit_coord` held) after every completed
    /// commit -- fsync waiters sleep here instead of driving the I/O
    /// themselves (`commit_until_covered`).
    pub commit_done: Condvar,
    /// The committer thread handle, if started. `None` = async commit
    /// disabled (`LFS_ASYNC_COMMIT=0`) or already stopped.
    pub committer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Phase 10 commit window seqlock: odd while a commit is writing
    /// the journal AND applying dirty blocks to their final on-disk
    /// locations (a non-atomic, multi-block mutation). Lock-free
    /// readers sample it before/after their read and retry if a commit
    /// overlapped -- they must never observe a half-applied tree.
    /// Without this, a reader that drops the staging lock just before
    /// `commit_active` could read a torn root block mid-apply (the
    /// 3.3 code was safe only because `&mut self` made the window
    /// unreachable).
    pub commit_seq: AtomicU64,
    /// Monotonic count of COMPLETED commits (incremented while still
    /// holding the staging lock, i.e. after `commit_tx`'s journal +
    /// apply + syncs all returned). The group-commit protocol: a thread
    /// records this value BEFORE staging (under the staging lock, so
    /// no commit can interleave), stages, then waits until the counter
    /// advances -- any commit ending after its staging MUST have taken
    /// the transaction after its staging (commits serialize on the
    /// same lock), so it includes the thread's bytes. Concurrent
    /// fsyncs coalesce into one journal run + one set of syncs instead
    /// of N -- the WAFL/PostgreSQL group-commit shape.
    pub commit_end: AtomicU64,
    pub inode_cache: InodeCache,
    pub key_manager: Mutex<KeyManager>,
    /// Phase 10 write-back intake cache: parallel `write()` calls,
    /// batched staging. See [`page_cache`] module docs.
    pub page_cache: PageCache,
}

impl SharedCore {
    /// `Superblock` is `Copy`: a read-lock + copy is a cheap,
    /// consistent snapshot of the mount geometry. Callers copy the
    /// fields they need at operation entry, before building contexts.
    pub fn sb(&self) -> Superblock {
        *self.superblock.read().unwrap()
    }

    /// Write access to the superblock for the (rare) in-op mutators:
    /// the inode allocator's free-list pop (create/mkdir). Only legal
    /// while already holding the staging lock -- lock order
    /// active_tx -> superblock-write, never the reverse.
    pub(crate) fn sb_write(&self) -> std::sync::RwLockWriteGuard<'_, Superblock> {
        self.superblock.write().unwrap()
    }

    /// Phase 9 (metadata CoW): finish a transaction whose frozen-tree
    /// roots may have moved. After the journal commit (which fsyncs
    /// every block the new roots point at), sync the transaction's
    /// root cells into the superblock and persist the superblock to
    /// all three slots so a remount finds the live tree. Commits that
    /// moved no root (the snapshot-free common case) are exactly the
    /// 3.2 behavior -- no extra writes.
    ///
    /// Phase 11: takes `&Transaction` (the frozen group is an
    /// `Arc<Transaction>` owned by the pending list while it commits)
    /// and is called by `commit_one` UNDER `commit_io`, without the
    /// staging lock.
    pub(crate) fn commit_tx(&self, tx: &Transaction) -> std::io::Result<()> {
        let sb_snapshot = self.sb();
        // Phase 11 fix: commit errors were swallowed here -- a failed
        // apply (device error, ENOSPC) left the group HALF-APPLIED
        // while the caller retired it as consistent (readers then
        // see valid tree state pointing at data blocks that were never
        // written: zero pages inside valid extents). Surface it: the
        // mount is in trouble either way, but loudly and without
        // pretending the group landed.
        self.tx_manager.commit(&self.disk, &sb_snapshot, tx)?;
        let mut sb = self.superblock.write().unwrap();
        // 3.6 (live space accounting): fold this transaction's net
        // allocation into the superblock. Slot persistence still only
        // happens when a root moved (unchanged 3.5 economics); the
        // in-memory value is exact either way and rides the next
        // slot write.
        if tx.alloc_delta != 0 {
            sb.free_blocks = (sb.free_blocks as i64 - tx.alloc_delta).max(0) as u64;
        }
        if tx.root_cells.is_empty() {
            return Ok(());
        }
        if let Some(&r) = tx.root_cells.get(&crate::inode::tree::INODE_TREE_NODE_TYPE) {
            sb.inode_tree_root = r;
        }
        if let Some(&r) = tx.root_cells.get(&crate::directory::tree::DIR_TREE_NODE_TYPE) {
            sb.dir_tree_root = r;
        }
        if let Some(&r) =
            tx.root_cells.get(&crate::integrity::checksum_tree::CHECKSUM_TREE_NODE_TYPE)
        {
            sb.checksum_tree_root = r;
        }
        // 3.6 (Format Vault): the xattr/ACL tree and the clone registry
        // join the root-cell sync. Their B-trees publish root moves
        // through `ctx.set_root_cell` exactly like the three trees
        // above; a 3.5 build simply never consults these fields (the
        // feature-flag bits gate their presence).
        if let Some(&r) = tx.root_cells.get(&crate::fs::xattrs::XATTR_TREE_NODE_TYPE) {
            sb.xattr_tree_root = r;
        }
        if let Some(&r) = tx.root_cells.get(&crate::fs::clones::CLONE_TREE_NODE_TYPE) {
            sb.clone_tree_root = r;
        }
        // The refcount coverage tree (dedup shares + reflink pins) is
        // rooted in its own superblock field; its first-use init and
        // root moves publish through root cells like every other tree.
        if let Some(&r) = tx.root_cells.get(&crate::integrity::refcount::REFCOUNT_TREE_NODE_TYPE) {
            sb.refcount_tree_root = r;
        }
        // Per-inode spill trees root themselves INSIDE inode entries
        // (frozen by the inode-tree CoW), so they need no superblock
        // field. Persist the stamp high-water so a remount restarts
        // stamps above everything on disk.
        sb.node_generation = crate::btree::tree::node_gen_current();
        let sb_copy = *sb;
        drop(sb);
        let floor = tx.id;
        crate::ondisk::superblock::write_all_slots(&self.disk, &sb_copy, floor)
    }

    /// Phase 11: snapshot of the pending frozen groups (oldest first).
    /// Cheap: `Arc` clones. The list only changes inside the odd
    /// (quiesce..retire) seqlock window, so a snapshot taken in an even
    /// window is stable for the reader's lifetime; a reader that
    /// entered during an odd window re-validates the seqlock.
    pub(crate) fn snapshot_committing(&self) -> Vec<Arc<Transaction>> {
        lock_ok!(self.committing).clone()
    }

    /// Phase 11: is any group quiesced but not yet retired? (Commit
    /// waiters use this to decide "work is in flight somewhere --
    /// wait" vs "nothing staged -- drive or finish".)
    pub(crate) fn committing_is_empty(&self) -> bool {
        lock_ok!(self.committing).is_empty()
    }

    /// Phase 11 (group-coverage fix): is transaction `txid` still
    /// live -- either the active (staging) transaction or a quiesced
    /// group in flight? A caller whose bytes were staged into `txid`
    /// is durably covered exactly when this returns false: the group
    /// retires only after its journal + syncs + apply + root cells all
    /// landed (`commit_one`). This is the precise replacement for the
    /// 3.4 epoch mark, whose reasoning ("a commit ENDING after my
    /// staging took the transaction after my staging") was broken by
    /// the lock split: `commit_end` now increments after the I/O, so
    /// a writer can stage between another writer's quiesce and its
    /// commit-end -- reading as covered while its bytes are still
    /// uncommitted (false cover, durability hole).
    /// Phase 11: every currently-live transaction id (active +
    /// pending), for the fsync no-pages fallback (write-through
    /// staging may sit in any of them).
    pub(crate) fn live_txids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = lock_ok!(self.committing).iter().map(|t| t.id).collect();
        if let Some(t) = lock_ok!(self.active_tx).as_ref() {
            ids.push(t.id);
        }
        ids
    }

    pub(crate) fn tx_live(&self, txid: u64) -> bool {
        let active = lock_ok!(self.active_tx)
            .as_ref()
            .is_some_and(|t| t.id == txid);
        if active {
            return true;
        }
        lock_ok!(self.committing).iter().any(|t| t.id == txid)
    }

    /// Phase 11 QUIESCE: freeze the active transaction out of staging.
    /// Fast (in-memory): take it, mark the seqlock odd, push it into
    /// the pending list, release the staging lock. The I/O phases run
    /// WITHOUT the staging lock so writers stage the next group
    /// concurrently (the pipeline). Empty transactions are discarded
    /// silently -- they carry nothing and cover no waiter's bytes.
    ///
    /// Lock order: staging lock -> committing (push). Nobody takes
    /// them in reverse.
    fn quiesce_tx(&self) -> Option<Arc<Transaction>> {
        let mut guard = lock_ok!(self.active_tx);
        if let Some(tx) = guard.take() {
            self.tx_present.store(false, Ordering::Release);
            if tx.dirty_blocks.is_empty() {
                return None;
            }
            let frozen = Arc::new(tx);
            self.commit_seq.fetch_add(1, Ordering::Release); // odd: apply window opens
            lock_ok!(self.committing).push(Arc::clone(&frozen));
            self.pending_count.fetch_add(1, Ordering::Release);
            Some(frozen)
        } else {
            None
        }
    }

    /// Phase 11: one full commit cycle -- quiesce, then (under
    /// `commit_io`) journal + syncs + apply + root cells, then retire
    /// from the pending list and publish `commit_end` + wake waiters.
    /// Callable from the committer thread AND from any synchronous
    /// driver (threshold flushes, write-through, destroy, a stalled
    /// fsync's belt-and-braces self-drive). Returns `true` iff a
    /// transaction was committed.
    pub(crate) fn commit_one(&self) -> bool {
        // Phase 11 ordering fix: commit_io must cover the QUIESCE too,
        // not just the I/O. Content ordering is defined by quiesce
        // order (a later group stages on top of the earlier one's
        // overlay), so an earlier group's apply must land before a
        // later group's -- if the I/O lock were taken only after the
        // quiesce, a driver that quiesced SECOND could acquire
        // commit_io FIRST and apply the NEWER blocks, only for the
        // earlier group's apply to then stamp its OLDER block versions
        // over them (the live lost-update the flush oracle caught:
        // a stale create-era inode re-materializing after commits).
        // Staging still overlaps I/O -- `with_stage_ctx_marked` needs
        // only the staging lock, which this path holds for the
        // microsecond quiesce, not the I/O.
        let _io = lock_ok!(self.commit_io);
        let frozen = match self.quiesce_tx() {
            Some(t) => t,
            None => return false,
        };
        {
            // (inner scope kept for the delay knob + error path)
            // Debug knob (Phase 11 race hunt): stall the I/O phase to
            // widen the quiesce->retire window deterministically.
            if let Ok(ms) = std::env::var("LFS_COMMIT_DELAY_MS") {
                if let Ok(ms) = ms.parse::<u64>() {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
            if let Err(e) = self.commit_tx(&frozen) {
                // DO NOT retire: the group is (at best) half-applied.
                // Keep it pending -- the journal is durable (the WAL
                // sync precedes the apply), so a retry or the next
                // mount's recovery completes or discards it. Retiring
                // would publish a torn state as consistent.
                eprintln!(
                    "lfs: commit of tx {} failed ({e}); group held pending for retry/recovery",
                    frozen.id
                );
                return true;
            }
        }
        // Retire: the group's blocks are durable at their final
        // locations and its root cells are persisted; readers can go
        // to disk for them now.
        lock_ok!(self.committing).retain(|t| !Arc::ptr_eq(t, &frozen));
        self.pending_count.fetch_sub(1, Ordering::Release);
        self.commit_seq.fetch_add(1, Ordering::Release); // even: consistent
        self.commit_end.fetch_add(1, Ordering::Release); // group-commit epoch
        // Hold the coord lock across the notify: a waiter that checked
        // `commit_end` just before our increment and is about to sleep
        // holds this lock, so our notify cannot land between its check
        // and its wait. (Waiters also time out, so a missed wakeup
        // costs latency, never correctness.)
        {
            let _coord = lock_ok!(self.commit_coord);
            self.commit_done.notify_all();
        }
        true
    }

    /// 3.6 (self-heal scrub): stage metadata updates through the
    /// SHARED transaction and commit the group. The scrubber thread
    /// (and the conformance tools) drive their journal-consistent
    /// bookkeeping -- verification statuses, the bad-block ledger --
    /// through the same staging lock, transaction, and journal as
    /// every other writer, so there is exactly ONE writer per image
    /// at all times. Lock discipline mirrors `with_stage_ctx`:
    /// staging -> superblock-write inside `f`, never the reverse.
    pub fn stage_and_commit<R>(
        &self,
        now: u64,
        f: impl FnOnce(&mut TxContext<'_>, &Superblock) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        let mut guard = lock_ok!(self.active_tx);
        if guard.is_none() {
            *guard = Some(self.tx_manager.begin(now));
            self.tx_present.store(true, Ordering::Release);
        }
        let tx = guard.as_mut().expect("just began");
        let sb = self.sb();
        let pending = self.snapshot_committing();
        let out = {
            let mut ctx = TxContext::new(&self.disk, tx)
                .with_cow_barrier(sb.last_snapshot_generation)
                .with_pending(&pending);
            f(&mut ctx, &sb)?
        };
        drop(guard);
        self.commit_one();
        Ok(out)
    }

    /// Phase 11: start the background committer thread (idempotent).
    /// The thread holds a `Weak` reference, so dropping every
    /// `LionFS` exits it without joining. Called by `LionFS::new`
    /// unless `LFS_ASYNC_COMMIT=0`.
    pub(crate) fn start_committer(core: &Arc<SharedCore>) {
        let mut handle = lock_ok!(core.committer);
        if handle.is_some() {
            return;
        }
        let weak = Arc::downgrade(core);
        let spawned = std::thread::Builder::new()
            .name("lfs-committer".to_string())
            .spawn(move || Self::committer_main(weak));
        if let Ok(h) = spawned {
            *handle = Some(h);
        }
    }

    /// Phase 11: stop and join the committer thread (mount destroy).
    /// After this returns no commit is in flight and every future
    /// commit is caller-driven (exactly 3.4 semantics).
    pub(crate) fn stop_committer(&self) {
        let handle = lock_ok!(self.committer).take();
        if let Some(h) = handle {
            lock_ok!(self.commit_coord).shutdown = true;
            self.commit_wake.notify_all();
            let _ = h.join();
            lock_ok!(self.commit_coord).shutdown = false;
        }
    }

    /// The committer thread: greedily quiesce-and-commit everything
    /// staged, then sleep on the wake condvar (with a 20 ms timeout as
    /// a safety net for wakeups missed between `commit_one` and the
    /// wait). Exits when shutdown is set or the mount is dropped.
    fn committer_main(weak: std::sync::Weak<SharedCore>) {
        loop {
            let core = match weak.upgrade() {
                Some(c) => c,
                None => return, // mount dropped: nothing to commit for
            };
            while core.commit_one() {}
            let mut coord = lock_ok!(core.commit_coord);
            if coord.shutdown {
                return;
            }
            let (guard, _timed_out) = core
                .commit_wake
                .wait_timeout(coord, Duration::from_millis(20))
                .unwrap_or_else(|p| p.into_inner());
            coord = guard;
            drop(coord);
        }
    }

    /// Phase 11: the lock-free scratch read. A read through committed
    /// state that NEVER takes the staging lock: seqlock sample ->
    /// pending-overlay context -> re-validate (retry on any commit
    /// boundary). During an odd (commit-in-flight) window with a
    /// visible pending group, the read goes THROUGH the frozen overlay
    /// -- the post-quiesce view, a legal linearization point -- instead
    /// of spinning for the commit to finish.
    pub(crate) fn with_scratch_ctx<R>(
        &self,
        mut f: impl FnMut(&mut TxContext<'_>, &Superblock) -> R,
    ) -> R {
        let mut temp_tx = Transaction::new(0, 0);
        loop {
            let seq = self.commit_seq.load(Ordering::Acquire);
            if seq & 1 == 1 {
                let pending = self.snapshot_committing();
                if !pending.is_empty() {
                    let sb = self.sb();
                    let out = {
                        let mut ctx =
                            TxContext::new(&self.disk, &mut temp_tx).with_pending(&pending);
                        f(&mut ctx, &sb)
                    };
                    // A second group can quiesce behind the first: the
                    // epoch would have advanced. Stable => consistent.
                    if self.commit_seq.load(Ordering::Acquire) == seq {
                        return out;
                    }
                    continue;
                }
                // Odd with nothing visible yet: the quiesce is between
                // marking odd and pushing the group (a few
                // instructions). Yield and re-sample.
                std::thread::yield_now();
                continue;
            }
            // Even window: snapshot the pending list under its lock
            // (Phase 11 postmortem: the lock-free pending_count fast
            // path was removed while root-causing a live mixed-epoch
            // read race under heavy parallel load -- the always-locked
            // snapshot is the conservative, always-consistent form).
            let sb = self.sb();
            let pending = self.snapshot_committing();
            let out = {
                let mut ctx = TxContext::new(&self.disk, &mut temp_tx).with_pending(&pending);
                f(&mut ctx, &sb)
            };
            if self.commit_seq.load(Ordering::Acquire) == seq {
                return out;
            }
            // A commit started and finished while we read: the bytes we
            // saw may straddle the apply. Retry on the post-commit
            // state (the pending snapshot is re-taken, so the new group
            // is visible).
        }
    }

    /// Read an inode through the cache; on a miss, through the
    /// in-flight transaction's overlay if one exists (so reads see
    /// staged-but-uncommitted metadata, the 3.3 read semantics), else
    /// the lock-free scratch read (Phase 11: seqlock + pending
    /// overlays -- no staging lock on the miss path).
    pub(crate) fn get_inode(&self, ino: u64) -> std::io::Result<Inode> {
        if let Some(inode) = self.inode_cache.get(ino) {
            return Ok(inode);
        }
        {
            let mut guard = lock_ok!(self.active_tx);
            if let Some(tx) = guard.as_mut() {
                let sb = self.sb();
                let pending = self.snapshot_committing();
                let inode = {
                    let mut ctx = TxContext::new(&self.disk, tx).with_pending(&pending);
                    crate::inode::manager::InodeManager::read_inode(
                        &mut ctx, sb.inode_tree_root, ino,
                    )?
                };
                drop(guard);
                self.inode_cache.insert(ino, inode, false);
                return Ok(inode);
            }
        }
        let inode = self.with_scratch_ctx(|ctx, sb| {
            crate::inode::manager::InodeManager::read_inode(ctx, sb.inode_tree_root, ino)
        })?;
        self.inode_cache.insert(ino, inode, false);
        Ok(inode)
    }

    /// Builds the block-cipher context for `inode`, resolving its key
    /// (if any) through the key manager. Cheap when encryption and
    /// compression are both off (the common case): returns immediately
    /// without touching the key tree.
    ///
    /// Phase 10: locks the key manager internally (callers hold the
    /// staging lock; lock order active_tx -> key_manager is the only
    /// legal order, enforced by having no path that locks them the
    /// other way around).
    pub(crate) fn resolve_block_cipher_ctx(
        &self,
        ctx: &mut TxContext,
        inode: &Inode,
    ) -> std::io::Result<BlockCipherContext> {
        let sb = self.sb();
        let key = if inode.encryption_algo != 0 {
            self.key_manager
                .lock()
                .unwrap()
                .get_key(ctx, sb.key_tree_root, inode.key_id)?
        } else {
            None
        };
        Ok(BlockCipherContext {
            compression_algo: inode.compression_algo,
            encryption_algo: inode.encryption_algo,
            key,
            crypto_tree_root: sb.crypto_tree_root,
        })
    }

    pub(crate) fn get_bg_desc(&self) -> crate::ondisk::serialization::BlockGroupDescriptor {
        let sb = self.sb();
        crate::ondisk::serialization::BlockGroupDescriptor {
            bg_block_bitmap: sb.bitmap_start,
            bg_inode_bitmap: 0,
            bg_inode_table: sb.inode_table_start,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
            bg_padding: 0,
            bg_reserved: [0; 32],
        }
    }
}

pub struct LionFS {
    /// Shared, lock-protected mount state. Every `VfsOps` call goes
    /// through this; the shell below only owns mount lifecycle.
    pub core: Arc<SharedCore>,
    pub(crate) scrubber: crate::worker::scrubber::ScrubberWorker,
    pub(crate) image_path: String,
}

impl LionFS {
    pub fn new(mut disk: Disk, image_path: String) -> std::io::Result<Self> {
        let mut buffer = [0u8; BLOCK_SIZE];
        let mut candidates: Vec<Option<[u8; BLOCK_SIZE]>> =
            Vec::with_capacity(crate::ondisk::superblock::CANDIDATE_LOCATIONS.len());
        for &loc in &crate::ondisk::superblock::CANDIDATE_LOCATIONS {
            if disk.read_block(loc, &mut buffer).is_ok() {
                candidates.push(Some(buffer));
            } else {
                candidates.push(None);
            }
        }

        let superblock = match crate::ondisk::superblock::pick_best(&candidates) {
            Some(sb) => sb,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "No valid superblock found or all checksums failed",
                ))
            }
        };

        // 3.6 (Format Vault): the mount gate moves INTO the core --
        // previously only the mount CLI checked the version, so a
        // library consumer (or a tool) would happily mount a future
        // image and misinterpret unknown fields. Refuse on an
        // unreadable version AND on unknown fs_features bits: the
        // image may carry structures this build does not know.
        if !crate::common::version::is_mountable(superblock.version, superblock.fs_features) {
            let unknown = crate::common::version::unknown_features(superblock.fs_features);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "refusing to mount: on-disk format version {} (this build writes {}), \
unknown feature bits {unknown:#b} set",
                    superblock.version,
                    crate::common::version::CURRENT_VERSION
                ),
            ));
        }

        // Recover journal if any
        let highest_tx =
            crate::recovery::recovery::RecoveryManager::recover(&mut disk, &superblock)?;

        let post_recovery = crate::recovery::verify::verify_post_recovery(&disk, &superblock);
        if !post_recovery.is_healthy() {
            eprintln!(
                "Warning: post-recovery verification found issues: {:?}",
                post_recovery.issues
            );
        }

        let tx_manager = TransactionManager::new(&superblock);

        if highest_tx
            > tx_manager
                .current_tx_id
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            tx_manager
                .current_tx_id
                .store(highest_tx, std::sync::atomic::Ordering::SeqCst);
        }

        // Phase 9 (metadata CoW): mirror this image's snapshot barrier
        // onto the Disk so every writer context (vfs or bare) path-
        // copies frozen nodes while snapshots are live.
        disk.live_barrier.fetch_max(
            superblock.last_snapshot_generation,
            std::sync::atomic::Ordering::AcqRel,
        );

        // Phase 9 (metadata CoW): start the global node-write stamp
        // counter ABOVE every on-disk stamp, every persisted snapshot
        // barrier, and every transaction id, so a node restamped after
        // remount can never masquerade as pre-snapshot content.
        crate::btree::tree::node_gen_init(
            superblock
                .node_generation
                .max(superblock.last_snapshot_generation)
                .max(highest_tx)
                .max(1),
        );

        let core = Arc::new(SharedCore {
            disk: Arc::new(disk),
            superblock: RwLock::new(superblock),
            tx_manager,
            active_tx: Mutex::new(None),
            committing: Mutex::new(Vec::new()),
            tx_present: AtomicBool::new(false),
            commit_io: Mutex::new(()),
            commit_coord: Mutex::new(CommitCoord::default()),
            pending_count: AtomicU64::new(0),
            commit_wake: Condvar::new(),
            commit_done: Condvar::new(),
            committer: Mutex::new(None),
            commit_seq: AtomicU64::new(0),
            commit_end: AtomicU64::new(0),
            inode_cache: InodeCache::new(10000),
            key_manager: Mutex::new(KeyManager::new()),
            page_cache: PageCache::new(),
        });

        // Phase 11: the pipelined committer thread (disable with
        // LFS_ASYNC_COMMIT=0 for caller-driven-only commits, the 3.4
        // behavior, for A/B measurement and debugging).
        if std::env::var("LFS_ASYNC_COMMIT").as_deref() != Ok("0") {
            SharedCore::start_committer(&core);
        }

        let scrubber = crate::worker::scrubber::ScrubberWorker::new();
        // Background workers are initialized but waiting

        Ok(Self {
            core,
            scrubber,
            image_path,
        })
    }
}
