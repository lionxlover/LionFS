//! Metadata path-copy CoW tests (Phase 9, 3.3).
//!
//! The money property: a snapshot's recorded tree ROOTS keep their
//! exact contents while the live tree diverges -- inode entries, dir
//! name entries, and checksum entries are all frozen by construction,
//! and the live tree finds its own (moved) root even through a caller
//! whose superblock value predates the move.
//!
//! Before 3.3, snapshot creation deep-copied the inode tree (O(inodes)
//! per snapshot) and left the dir and checksum trees shared and
//! mutable, so snapshot reads had to disable checksum verification.

use crate::btree::tree::BTree;
use crate::disk::block_io::Disk;
use crate::fs::snapshots::SnapshotManager;
use crate::inode::tree::INODE_TREE_NODE_TYPE;
use crate::integrity::checksum_tree::{
    ChecksumTree, ChecksumTreeKey, ChecksumTreeValue, CHECKSUM_TREE_NODE_TYPE,
};
use crate::directory::tree::DirectoryTree;
use crate::ondisk::serialization::{Inode, Superblock, BLOCK_SIZE};
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;

fn zero_sb() -> Superblock {
    Superblock {
        block_size: 4096,
        next_ino: 3,
        ..unsafe { std::mem::zeroed() }
    }
}

/// Counting allocator: hands out sequential blocks and counts how many
/// were requested -- the white-box observable for "how many nodes did
/// the CoW pass copy".
struct CountingAlloc {
    next: u64,
    count: std::cell::Cell<u64>,
}

impl CountingAlloc {
    fn new(start: u64) -> Self {
        Self {
            next: start,
            count: std::cell::Cell::new(0),
        }
    }
    fn alloc(&mut self, _ctx: &mut TxContext) -> std::io::Result<u64> {
        let b = self.next;
        self.next += 1;
        self.count.set(self.count.get() + 1);
        Ok(b)
    }
    fn take(&self) -> u64 {
        self.count.replace(0)
    }
}

/// A fresh environment per test: real disk image, real trees, real
/// transaction contexts. `CowEnv` mirrors cow_tests.rs but keeps the
/// Disk reachable so the Disk-level barrier/frozen-root mirrors are
/// exercised (the 3.3 protections for bare-context callers).
struct CowEnv {
    disk: &'static mut Disk,
    tm: &'static TransactionManager,
    tx: &'static mut crate::transaction::transaction::Transaction,
}

fn setup(tag: &str) -> CowEnv {
    let path = std::env::temp_dir().join(format!("test_mcow_{tag}.img"));
    let mut disk = Disk::create(&path, 1024 * 1024 * 16).unwrap();
    let sb = zero_sb();
    let tm: &'static TransactionManager = Box::leak(Box::new(TransactionManager::new(&sb)));
    let tx: &'static mut crate::transaction::transaction::Transaction =
        Box::leak(Box::new(tm.begin(0)));
    let disk: &'static mut Disk = Box::leak(Box::new(disk));
    let mut ctx = TxContext::new(disk, tx);
    BTree::<u64, Inode>::init_empty(&mut ctx, 20, INODE_TREE_NODE_TYPE).unwrap();
    SnapshotManager::init_empty(&mut ctx, 24).unwrap();
    ChecksumTree::init_empty(&mut ctx, 28).unwrap();
    DirectoryTree::init_empty(&mut ctx, 30).unwrap();
    CowEnv { disk, tm, tx }
}

fn fresh_inode(ino: u64) -> Inode {
    Inode {
        ino,
        mode: 0o100644,
        size: 0,
        extent_count: 0,
        ..unsafe { std::mem::zeroed() }
    }
}

fn insert_inode(env: &mut CowEnv, ino: u64, size: u64) {
    let mut node = fresh_inode(ino);
    node.size = size;
    let mut ca = CountingAlloc::new(400);
    let mut tree = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
    tree.insert(&mut env_ctx(env), ino, node, |c| ca.alloc(c)).unwrap();
}

/// Build a context over the shared disk + a FRESH transaction (a
/// "bare" caller: no vfs barrier plumb, no shared tx cells).
fn env_ctx(env: &mut CowEnv) -> TxContext<'_> {
    // Reborrow the leaked statics at the shorter lifetime of `env`.
    let disk: &mut Disk = &mut *env.disk;
    let tx: &mut crate::transaction::transaction::Transaction = &mut *env.tx;
    TxContext::new(disk, tx)
}

/// THE money test (metadata): the snapshot's recorded inode-tree root
/// keeps the pre-snapshot inode even after the live tree's entry is
/// updated in place, BECAUSE the update path-copied the frozen leaf
/// first. The live tree finds its own moved root through the Disk
/// mirror even though every handle in this test was constructed from
/// the ORIGINAL (stale) superblock root.
#[test]
fn metadata_cow_freezes_inode_view() {
    let mut env = setup("inode");

    // 1. Two inodes in the tree, stamped before any snapshot.
    insert_inode(&mut env, 1, 100);
    insert_inode(&mut env, 2, 200);

    // 2. Snapshot: records the CURRENT root (root 20). O(1) metadata:
    //    no deep copy, no re-insertion walk.
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(400);
        let mut snap = SnapshotManager::new(24);
        let mut sb = zero_sb();
        sb.inode_tree_root = 20;
        sb.snapshot_tree_root = 24;
        sb.next_ino = 3;
        snap.create_snapshot(&mut ctx, &mut sb, 7, 0, &mut |c| ca.alloc(c))
            .unwrap();
        // Exactly ONE allocation: the snapshot record itself. The 3.2
        // design also allocated a full inode-tree copy per snapshot.
        assert_eq!(
            ca.take(), 1,
            "O(1) metadata: creation allocates only the record node (3.2 allocated a whole inode-tree copy too)"
        );
    }

    // 3. Live update of inode 1 THROUGH A FRESH BARE CONTEXT (worst
    //    case: new tx, no ctx barrier, handle built from the stale
    //    superblock root 20). The Disk-level barrier + frozen-root
    //    mirror must make this both correct and CoW-safe.
    {
        let mut ca = CountingAlloc::new(500);
        let mut ctx = env_ctx(&mut env);
        let mut tree = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
        let mut node = fresh_inode(1);
        node.size = 9_999;
        tree.insert(&mut ctx, 1, node, |c| ca.alloc(c)).unwrap();
        let copied = ca.take();
        assert!(copied >= 1, "the frozen leaf must be path-copied, got {copied} copies");

        // The live tree sees the update -- through the moved root.
        let seen: Option<Inode> = tree.lookup(&mut ctx, &1).unwrap();
        assert_eq!(seen.unwrap().size, 9_999, "live tree must see the update");

        // A brand-new handle built from the STALE root value also sees
        // it (Disk frozen-root mirror), not just this handle.
        let fresh_handle = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
        let seen2: Option<Inode> = fresh_handle.lookup(&mut ctx, &1).unwrap();
        assert_eq!(
            seen2.unwrap().size,
            9_999,
            "a stale-superblock handle must find the moved root via the Disk mirror"
        );
    }

    // 4. The SNAPSHOT's recorded root (20) still returns the OLD view.
    {
        let mut ctx = env_ctx(&mut env);
        let snap = SnapshotManager::new(24);
        let old = snap
            .read_snapshot_inode(&mut ctx, 7, 1)
            .unwrap()
            .expect("snapshot holds inode 1");
        assert_eq!(old.size, 100, "snapshot inode view must be frozen");
        let old2 = snap.read_snapshot_inode(&mut ctx, 7, 2).unwrap().unwrap();
        assert_eq!(old2.size, 200);
    }
}

/// The second post-snapshot write into the SAME leaf copies nothing:
/// copies are once per snapshot epoch, not per write.
#[test]
fn cow_copies_once_per_epoch() {
    let mut env = setup("epoch");
    insert_inode(&mut env, 1, 10);
    insert_inode(&mut env, 2, 20);

    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(400);
        let mut snap = SnapshotManager::new(24);
        let mut sb = zero_sb();
        sb.inode_tree_root = 20;
        sb.snapshot_tree_root = 24;
        sb.next_ino = 3;
        snap.create_snapshot(&mut ctx, &mut sb, 7, 0, &mut |c| ca.alloc(c))
            .unwrap();
    }

    // First post-snapshot mutation: the frozen leaf (and only the leaf
    // -- the tree is one node deep) is copied once.
    let first = {
        let mut ca = CountingAlloc::new(500);
        let mut ctx = env_ctx(&mut env);
        let mut tree = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
        let mut node = fresh_inode(1);
        node.size = 11;
        tree.insert(&mut ctx, 1, node, |c| ca.alloc(c)).unwrap();
        ca.take()
    };
    assert!(first >= 1, "first write after snapshot must copy the frozen leaf");

    // Second mutation (different key, same leaf): zero copies.
    let second = {
        let mut ca = CountingAlloc::new(600);
        let mut ctx = env_ctx(&mut env);
        let mut tree = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
        let mut node = fresh_inode(2);
        node.size = 22;
        tree.insert(&mut ctx, 2, node, |c| ca.alloc(c)).unwrap();
        ca.take()
    };
    assert_eq!(
        second, 0,
        "the copied leaf is private: a second write in the same epoch must not copy again"
    );

    // The snapshot still sees the original sizes.
    {
        let mut ctx = env_ctx(&mut env);
        let snap = SnapshotManager::new(24);
        assert_eq!(snap.read_snapshot_inode(&mut ctx, 7, 1).unwrap().unwrap().size, 10);
        assert_eq!(snap.read_snapshot_inode(&mut ctx, 7, 2).unwrap().unwrap().size, 20);
    }
}

/// Deleting the newest snapshot lowers the barrier to the oldest live
/// one: nodes stamped between them become mutable in place again
/// (observable as zero copies), while the OLDER snapshot's view stays
/// frozen.
#[test]
fn barrier_recompute_on_delete() {
    let mut env = setup("barrier");
    insert_inode(&mut env, 1, 10);

    // S1 at barrier b1.
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(400);
        let mut snap = SnapshotManager::new(24);
        let mut sb = zero_sb();
        sb.inode_tree_root = 20;
        sb.snapshot_tree_root = 24;
        sb.next_ino = 3;
        snap.create_snapshot(&mut ctx, &mut sb, 1, 0, &mut |c| ca.alloc(c))
            .unwrap();
    }
    // Post-S1 write: copies the frozen leaf (now private, stamped > b1).
    {
        let mut ca = CountingAlloc::new(500);
        let mut ctx = env_ctx(&mut env);
        let mut tree = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
        let mut node = fresh_inode(1);
        node.size = 11;
        tree.insert(&mut ctx, 1, node, |c| ca.alloc(c)).unwrap();
        assert!(ca.take() >= 1);
    }
    // S2 at barrier b2 > b1.
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(520);
        let mut snap = SnapshotManager::new(24);
        let mut sb = zero_sb();
        sb.inode_tree_root = 20;
        sb.snapshot_tree_root = 24;
        sb.next_ino = 3;
        snap.create_snapshot(&mut ctx, &mut sb, 2, 0, &mut |c| ca.alloc(c))
            .unwrap();
    }
    // Delete S2: barrier falls back to b1. The leaf (stamped > b1, it
    // was copied after S1) is private w.r.t. S1 -- zero copies now.
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(540);
        let mut snap = SnapshotManager::new(24);
        let mut sb = zero_sb();
        sb.inode_tree_root = 20;
        sb.snapshot_tree_root = 24;
        sb.next_ino = 3;
        sb.refcount_tree_root = 0; // no data pins were taken (no extents)
        snap.delete_snapshot(&mut ctx, &mut sb, 2, &mut |c| ca.alloc(c))
            .unwrap();
    }
    let after_delete = {
        let mut ca = CountingAlloc::new(600);
        let mut ctx = env_ctx(&mut env);
        let mut tree = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
        let mut node = fresh_inode(1);
        node.size = 12;
        tree.insert(&mut ctx, 1, node, |c| ca.alloc(c)).unwrap();
        ca.take()
    };
    assert_eq!(
        after_delete, 0,
        "deleting the newest snapshot must un-freeze nodes only it could reach"
    );

    // S1's view is untouched by any of the above.
    {
        let mut ctx = env_ctx(&mut env);
        let snap = SnapshotManager::new(24);
        assert_eq!(
            snap.read_snapshot_inode(&mut ctx, 1, 1).unwrap().unwrap().size,
            10,
            "the OLDER snapshot's view must survive S2's deletion"
        );
    }
}

/// The checksum tree is frozen by CoW (3.3): a snapshot's csum entry
/// keeps the pre-snapshot value after the live entry is updated, so
/// snapshot reads can VERIFY their (pinned) data against it.
#[test]
fn checksum_tree_frozen_by_snapshot() {
    let mut env = setup("csum");

    let key = ChecksumTreeKey {
        object_id: 9,
        logical_block: 4,
    };
    let csum_val = |tag: u8| ChecksumTreeValue {
        physical_block: 77,
        checksum_bytes: [tag; 32],
        generation: 1,
        algorithm_id: 1,
        verification_status: 0,
        padding: [0; 6],
    };

    // Insert the ORIGINAL checksum entry.
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(400);
        let mut ctree = ChecksumTree::new(28);
        ctree
            .insert_checksum(&mut ctx, key, csum_val(0xAA), &mut |c| ca.alloc(c))
            .unwrap();
    }

    // Snapshot (records csum root 28).
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(420);
        let mut snap = SnapshotManager::new(24);
        let mut sb = zero_sb();
        sb.inode_tree_root = 20;
        sb.snapshot_tree_root = 24;
        sb.checksum_tree_root = 28;
        sb.next_ino = 3;
        snap.create_snapshot(&mut ctx, &mut sb, 5, 0, &mut |c| ca.alloc(c))
            .unwrap();
    }

    // Live update of the SAME key (the block was rewritten: new csum).
    {
        let mut ca = CountingAlloc::new(500);
        let mut ctx = env_ctx(&mut env);
        let mut ctree = ChecksumTree::new(28);
        ctree
            .insert_checksum(&mut ctx, key, csum_val(0xBB), &mut |c| ca.alloc(c))
            .unwrap();
        assert!(ca.take() >= 1, "the frozen csum leaf must be path-copied");
        let live = ctree.lookup_checksum(&mut ctx, &key).unwrap().unwrap();
        assert_eq!(live.checksum_bytes[0], 0xBB, "live tree sees the new csum");
    }

    // The snapshot's checksum lookup returns the ORIGINAL value -- the
    // capability 3.2 explicitly lacked (snapshot reads had to pass
    // checksum_tree_root = 0, verification off).
    {
        let mut ctx = env_ctx(&mut env);
        let snap = SnapshotManager::new(24);
        let frozen = snap
            .read_snapshot_csum(&mut ctx, 5, 9, 4)
            .unwrap()
            .expect("snapshot must hold the csum entry");
        assert_eq!(frozen.checksum_bytes[0], 0xAA, "snapshot csum view must be frozen");
    }
}

/// The dir-name tree is frozen by CoW (3.3): name resolution against a
/// snapshot returns the pre-snapshot mapping after new entries are
/// added live.
#[test]
fn dir_tree_frozen_by_snapshot() {
    let mut env = setup("dir");

    // Insert "old.txt" -> ino 11 into the dir tree.
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(400);
        let mut dtree = DirectoryTree::new(30);
        dtree
            .insert(&mut ctx, "old.txt", 11, 0, |c| ca.alloc(c))
            .unwrap();
    }

    // Snapshot (records dir root 30).
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(420);
        let mut snap = SnapshotManager::new(24);
        let mut sb = zero_sb();
        sb.inode_tree_root = 20;
        sb.snapshot_tree_root = 24;
        sb.dir_tree_root = 30;
        sb.next_ino = 3;
        snap.create_snapshot(&mut ctx, &mut sb, 5, 0, &mut |c| ca.alloc(c))
            .unwrap();
    }

    // Live: add "new.txt" -> ino 12.
    {
        let mut ca = CountingAlloc::new(500);
        let mut ctx = env_ctx(&mut env);
        let mut dtree = DirectoryTree::new(30);
        dtree
            .insert(&mut ctx, "new.txt", 12, 0, |c| ca.alloc(c))
            .unwrap();
        assert!(ca.take() >= 1, "the frozen dir leaf must be path-copied");
        assert!(dtree.lookup(&mut ctx, "new.txt").unwrap().is_some(), "live sees new.txt");
    }

    // Snapshot: old.txt resolves, new.txt does not.
    {
        let mut ctx = env_ctx(&mut env);
        let snap = SnapshotManager::new(24);
        let old = snap
            .read_snapshot_dir_entry(&mut ctx, 5, "old.txt")
            .unwrap()
            .expect("snapshot must resolve the pre-snapshot name");
        assert_eq!(old.ino, 11);
        let new = snap.read_snapshot_dir_entry(&mut ctx, 5, "new.txt").unwrap();
        assert!(new.is_none(), "post-snapshot name must not exist in the snapshot view");
    }
}

/// Many sequential post-snapshot writes through the fast-append path
/// stay correct: the fast path must refuse a FROZEN cached leaf and
/// fall to the descent (which copies it), and after the copy the cache
/// must track the copy. Regression guard for the interaction of the
/// 3.2 fast-append cache with the 3.3 CoW pass.
#[test]
fn fast_append_respects_cow() {
    let mut env = setup("fast");

    // Fill the csum tree with enough entries that a second handle
    // would find a rightmost-leaf candidate (the fast path is
    // per-handle, so a single handle drives the loop).
    let mut live_root = 28u64;
    {
        let mut ctx = env_ctx(&mut env);
        let mut ctree = ChecksumTree::new(28);
        let mut ca = CountingAlloc::new(400);
        let val = ChecksumTreeValue {
            physical_block: 1,
            checksum_bytes: [7; 32],
            generation: 1,
            algorithm_id: 1,
            verification_status: 0,
            padding: [0; 6],
        };
        for lb in 0..40u64 {
            ctree
                .insert_checksum(
                    &mut ctx,
                    ChecksumTreeKey {
                        object_id: 3,
                        logical_block: lb,
                    },
                    val,
                    &mut |c| ca.alloc(c),
                )
                .unwrap();
        }
        live_root = ctx.effective_root(CHECKSUM_TREE_NODE_TYPE, 28);
    }

    // Snapshot after the tree is warm (rightmost leaf cached in a
    // live handle is what we test next).
    {
        let mut ctx = env_ctx(&mut env);
        let mut ca = CountingAlloc::new(460);
        let mut snap = SnapshotManager::new(24);
        let mut sb = zero_sb();
        sb.inode_tree_root = 20;
        sb.snapshot_tree_root = 24;
        sb.checksum_tree_root = live_root;
        sb.next_ino = 3;
        snap.create_snapshot(&mut ctx, &mut sb, 9, 0, &mut |c| ca.alloc(c))
            .unwrap();
    }

    // Keep appending through the SAME handle: the first insert hits a
    // frozen cached leaf (fast path must bail); after the CoW copy the
    // cache refreshes onto the private copy and the fast path resumes.
    {
        let mut ca = CountingAlloc::new(600);
        let mut ctx = env_ctx(&mut env);
        let mut ctree = ChecksumTree::new(live_root);
        let val = ChecksumTreeValue {
            physical_block: 2,
            checksum_bytes: [9; 32],
            generation: 1,
            algorithm_id: 1,
            verification_status: 0,
            padding: [0; 6],
        };
        for lb in 40u64..120u64 {
            ctree
                .insert_checksum(
                    &mut ctx,
                    ChecksumTreeKey {
                        object_id: 3,
                        logical_block: lb,
                    },
                    val,
                    &mut |c| ca.alloc(c),
                )
                .unwrap();
        }
        // Every entry must be present in the live view.
        for lb in (0u64..120).step_by(7) {
            let hit = ctree
                .lookup_checksum(
                    &mut ctx,
                    &ChecksumTreeKey {
                        object_id: 3,
                        logical_block: lb,
                    },
                )
                .unwrap();
            assert!(hit.is_some(), "live csum entry {lb} must exist after CoW appends");
        }
    }

    // The snapshot's view still ends at the pre-snapshot key range.
    {
        let mut ctx = env_ctx(&mut env);
        let snap = SnapshotManager::new(24);
        let pre = snap.read_snapshot_csum(&mut ctx, 9, 3, 39).unwrap();
        assert!(pre.is_some(), "pre-snapshot entry must be in the frozen view");
        let post = snap.read_snapshot_csum(&mut ctx, 9, 3, 40).unwrap();
        assert!(post.is_none(), "post-snapshot entry must NOT be in the frozen view");
    }
}
