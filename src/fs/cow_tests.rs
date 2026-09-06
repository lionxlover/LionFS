//! Data-path CoW tests (Phase 6 completed, 3.2).
//!
//! The money test is `snapshot_write_isolation`: what a snapshot read
//! sees must not change when the live file is overwritten afterwards.
//! Before 3.2 this failed silently -- snapshots pointed at blocks that
//! the write path modified in place.

use crate::btree::tree::BTree;
use crate::disk::block_io::Disk;
use crate::file::writer::FileManager;
use crate::fs::snapshots::SnapshotManager;
use crate::inode::tree::INODE_TREE_NODE_TYPE;
use crate::integrity::refcount::RefCountManager;
use crate::ondisk::serialization::{BlockGroupDescriptor, Inode, Superblock};
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;
use crate::ondisk::serialization::BLOCK_SIZE;

fn zero_sb() -> Superblock {
    Superblock {
        block_size: 4096,
        next_ino: 3,
        ..unsafe { std::mem::zeroed() }
    }
}

fn bg() -> BlockGroupDescriptor {
    BlockGroupDescriptor {
        bg_block_bitmap: 40,
        bg_inode_bitmap: 0,
        bg_inode_table: 0,
        bg_free_blocks_count: 0,
        bg_free_inodes_count: 0,
        bg_used_dirs_count: 0,
        bg_padding: 0,
        bg_reserved: [0; 32],
    }
}

fn inactive_cipher() -> BlockCipherContext {
    BlockCipherContext {
        compression_algo: 0,
        encryption_algo: 0,
        key: None,
        crypto_tree_root: 0,
    }
}

fn fresh_inode() -> Inode {
    Inode {
        ino: 2,
        mode: 0o100644,
        size: 0,
        extent_count: 0,
        ..unsafe { std::mem::zeroed() }
    }
}

fn write_inode_fixture(ctx: &mut TxContext, inode: &Inode) {
    let mut tree = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
    let mut next = 300u64;
    let mut alloc = move |_c: &mut TxContext| {
        let b = next;
        next += 1;
        Ok(b)
    };
    tree.insert(ctx, inode.ino, *inode, &mut alloc).unwrap();
}

/// Common environment builder: disk + sb + trees + free bitmap.
struct CowEnv {
    ctx: TxContext<'static>,
    _sb: &'static Superblock,
}

fn setup(tag: &str, with_snapshot_tree: bool) -> CowEnv {
    let path = std::env::temp_dir().join(format!("test_cow_{tag}.img"));
    let mut disk = Disk::create(&path, 1024 * 1024 * 16).unwrap();
    let mut sb = zero_sb();
    sb.inode_tree_root = 20;
    sb.checksum_tree_root = 22;
    sb.snapshot_tree_root = if with_snapshot_tree { 24 } else { 0 };
    sb.next_ino = 3;
    disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
    let sb_static: &'static Superblock = Box::leak(Box::new(sb));
    let tm: &'static TransactionManager = Box::leak(Box::new(TransactionManager::new(sb_static)));
    let tx: &'static mut crate::transaction::transaction::Transaction =
        Box::leak(Box::new(tm.begin(0)));
    let disk: &'static mut Disk = Box::leak(Box::new(disk));
    let mut ctx = TxContext::new(disk, tx);
    BTree::<u64, Inode>::init_empty(&mut ctx, 20, INODE_TREE_NODE_TYPE).unwrap();
    crate::integrity::checksum_tree::ChecksumTree::init_empty(&mut ctx, 22).unwrap();
    if with_snapshot_tree {
        SnapshotManager::init_empty(&mut ctx, 24).unwrap();
    }
    RefCountManager::init_empty(&mut ctx, 26).unwrap();
    let mut bitmap = vec![0u8; 4096];
    for byte in bitmap.iter_mut().take(8) {
        *byte = 0xFF;
    }
    ctx.write_block(40, &bitmap).unwrap();
    CowEnv { ctx, _sb: sb_static }
}

/// THE test: a snapshot's read view survives overwrites of the live
/// file. Before 3.2 the write path modified the shared block in place
/// and the "snapshot" read the NEW bytes.
#[test]
fn snapshot_write_isolation() {
    let mut env = setup("iso", true);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;

    // 1. Write one block: "A" x 4096.
    let mut inode = fresh_inode();
    let data_a = vec![b'A'; BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 0, 0, &cctx, &mut inode, 0, &data_a).unwrap();
    write_inode_fixture(ctx, &inode);
    let pinned_phys = inode.extents[0].physical_start;

    // 2. Snapshot (freezes the inode view; protects the data).
    //    Phase 11: this fixture has the checksum tree ON, so the
    //    snapshot is BIRTH mode -- no pins are created; protection is
    //    the per-block birth generations recorded by the writes (the
    //    write path derives "must redirect" from birth <= barrier).
    let mut snap = SnapshotManager::new(24);
    let mut next_node = 300u64;
    let mut alloc = move |_c: &mut TxContext| {
        let b = next_node;
        next_node += 1;
        Ok(b)
    };
    let mut sb2 = zero_sb();
    sb2.inode_tree_root = 20;
    sb2.checksum_tree_root = 22;
    sb2.snapshot_tree_root = 24;
    sb2.refcount_tree_root = 26;
    sb2.next_ino = 3;
    snap.create_snapshot(ctx, &mut sb2, 1, 0, &mut alloc).unwrap();
    let rc = RefCountManager::new(26);
    // Birth mode: no pin was created (that is the O(1) point), and the
    // protection lives in the csum record's birth generation.
    assert!(
        !rc.is_pinned(ctx, pinned_phys).unwrap(),
        "birth-mode snapshots pin nothing -- creation is O(1)"
    );
    let csum = crate::integrity::checksum_tree::ChecksumTree::new(22);
    let rec = csum
        .lookup_checksum(
            ctx,
            &crate::integrity::checksum_tree::ChecksumTreeKey {
                object_id: 2,
                logical_block: 0,
            },
        )
        .unwrap()
        .expect("write must record a csum birth entry");
    assert!(
        rec.generation <= sb2.last_snapshot_generation,
        "the pre-snapshot write's birth must be at-or-below the barrier"
    );

    // 3. Overwrite the SAME logical block with "B" x 4096 -- must CoW.
    let data_b = vec![b'B'; BLOCK_SIZE];
    let mut live = inode;
    FileManager::write_file(ctx, &bg(), bpg, 22, 26, 0, &cctx, &mut live, 0, &data_b).unwrap();

    // The live mapping moved; the original block still holds "A".
    let live_phys = live.extents[0].physical_start;
    assert_ne!(live_phys, pinned_phys, "live write must redirect (CoW), not overwrite");
    let mut old_bytes = [0u8; BLOCK_SIZE];
    ctx.read_block(pinned_phys, &mut old_bytes).unwrap();
    assert!(old_bytes.iter().all(|&b| b == b'A'), "pinned block must retain snapshot content");
    let mut live_bytes = [0u8; BLOCK_SIZE];
    ctx.read_block(live_phys, &mut live_bytes).unwrap();
    assert!(live_bytes.iter().all(|&b| b == b'B'), "live block holds the new content");

    // 4. The SNAPSHOT's frozen inode still points at the original block.
    let snap_inode = snap
        .read_snapshot_inode(ctx, 1, 2)
        .unwrap()
        .expect("snapshot must hold the inode");
    assert_eq!(snap_inode.extents[0].physical_start, pinned_phys);
    assert_eq!(snap_inode.size, BLOCK_SIZE as u64);
    assert_eq!(live.extents[0].physical_start, live_phys);
}

/// CoW applies per-block: overwriting one block of a multi-block run
/// splits the extent and redirects ONLY that block; neighbors keep
/// their mappings.
#[test]
fn cow_redirect_splits_extents_not_neighbors() {
    let mut env = setup("split", false);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;

    // A 4-block sequential file: one extent run of length 4.
    let mut inode = fresh_inode();
    let data = vec![b'C'; 4 * BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 0, 0, &cctx, &mut inode, 0, &data).unwrap();
    assert_eq!(inode.extent_count, 1, "sequential write must produce one run");
    let run_ps = inode.extents[0].physical_start;

    // Pin the run manually (as a snapshot would).
    let mut rc = RefCountManager::new(26);
    let mut next_node = 300u64;
    let mut alloc = move |_c: &mut TxContext| {
        let b = next_node;
        next_node += 1;
        Ok(b)
    };
    rc.pin_range(ctx, run_ps, 4, &mut alloc).unwrap();

    // Overwrite ONLY logical block 2.
    let mut live = inode;
    let patch = vec![b'D'; BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 26, 0, &cctx, &mut live, 2 * BLOCK_SIZE as u64, &patch)
        .unwrap();

    // Block 2's bytes moved somewhere new; blocks 0,1,3 kept their
    // original physical homes (still inside the pinned run).
    let p2_new = FileManager::resolve_physical_block(ctx, &live, 2).unwrap();
    assert_ne!(p2_new, run_ps + 2, "block 2 must be redirected");
    assert_eq!(
        FileManager::resolve_physical_block(ctx, &live, 0).unwrap(),
        run_ps,
        "neighbor keeps its mapping"
    );
    assert_eq!(
        FileManager::resolve_physical_block(ctx, &live, 3).unwrap(),
        run_ps + 3,
        "neighbor keeps its mapping"
    );
    let mut at_new = [0u8; BLOCK_SIZE];
    ctx.read_block(p2_new, &mut at_new).unwrap();
    assert!(at_new.iter().all(|&b| b == b'D'));
    let mut at_old = [0u8; BLOCK_SIZE];
    ctx.read_block(run_ps + 2, &mut at_old).unwrap();
    assert!(at_old.iter().all(|&b| b == b'C'));
}

/// No-coverage fast path: with no pins, writes are byte-identical to
/// the pre-CoW behavior (in-place, no redirects).
#[test]
fn unpinned_writes_stay_in_place() {
    let mut env = setup("unpinned", false);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;

    let mut inode = fresh_inode();
    let v1 = vec![b'X'; BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 26, 0, &cctx, &mut inode, 0, &v1).unwrap();
    let phys = inode.extents[0].physical_start;
    let v2 = vec![b'Y'; BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 26, 0, &cctx, &mut inode, 0, &v2).unwrap();
    assert_eq!(inode.extents[0].physical_start, phys);
    let mut got = [0u8; BLOCK_SIZE];
    ctx.read_block(phys, &mut got).unwrap();
    assert!(got.iter().all(|&b| b == b'Y'));
}

/// G1 (3.2) dedup wiring: two inodes writing identical full blocks
/// share ONE physical block; both read back identical bytes; only one
/// allocation happened.
#[test]
fn dedup_shares_identical_blocks() {
    let mut env = setup("dedup_share", false);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;
    // Dedup index at tree root 28 (metadata zone).
    crate::fs::dedupe::DedupeTree::init_empty(ctx, 28).unwrap();

    crate::file::writer::set_dedup_enabled(true);
    let _off = PanicResetDedup;

    // Inode 2 writes one block of "S".
    let mut a = fresh_inode();
    let s_block = vec![b'S'; BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 0, 28, &cctx, &mut a, 0, &s_block).unwrap();
    let phys_a = a.extents[0].physical_start;

    // Inode 3 writes the SAME content.
    let mut b_inode = fresh_inode();
    b_inode.ino = 3;
    FileManager::write_file(ctx, &bg(), bpg, 22, 26, 28, &cctx, &mut b_inode, 0, &s_block).unwrap();
    let phys_b = b_inode.extents[0].physical_start;

    assert_eq!(phys_a, phys_b, "identical content must share the physical block");

    // Both read back the same bytes.
    let mut got = [0u8; BLOCK_SIZE];
    ctx.read_block(phys_b, &mut got).unwrap();
    assert!(got.iter().all(|&x| x == b'S'));

    // The share is pinned: an overwrite by EITHER inode must redirect.
    let new_content = vec![b'T'; BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 26, 28, &cctx, &mut a, 0, &new_content).unwrap();
    assert_ne!(
        a.extents[0].physical_start, phys_a,
        "overwrite of a dedup-shared block must CoW"
    );
    // And the shared original still holds "S" for inode 3.
    let mut still_s = [0u8; BLOCK_SIZE];
    ctx.read_block(phys_b, &mut still_s).unwrap();
    assert!(still_s.iter().all(|&x| x == b'S'));
}

/// A stale index entry (points at a block whose content no longer
/// matches the hash) must degrade to a normal write -- never to a
/// wrong share.
#[test]
fn dedup_stale_entry_degrades_to_new_write() {
    let mut env = setup("dedup_stale", false);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;
    crate::fs::dedupe::DedupeTree::init_empty(ctx, 28).unwrap();

    // Write a block of "U" through inode 2 (records hash(U) -> phys).
    crate::file::writer::set_dedup_enabled(true);
    let _off = PanicResetDedup;
    let mut a = fresh_inode();
    let u_block = vec![b'U'; BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 0, 28, &cctx, &mut a, 0, &u_block).unwrap();
    let phys_u = a.extents[0].physical_start;

    // Corrupt the index by hand: point hash(U) at a block of zeros.
    let hash = crate::fs::dedupe::DeduplicationManager::hash_block(&u_block);
    let mut dtree = crate::fs::dedupe::DedupeTree::new(28);
    let mut next_node = 300u64;
    let mut alloc = move |_c: &mut TxContext| {
        let b = next_node;
        next_node += 1;
        Ok(b)
    };
    dtree.insert_new(ctx, hash, 60, &mut alloc).unwrap(); // overwrite entry

    // Inode 3 writes "U" content: verify-on-share must REJECT the stale
    // pointer (block 60 is zeros) and fall back to a fresh write.
    let mut c = fresh_inode();
    c.ino = 3;
    FileManager::write_file(ctx, &bg(), bpg, 22, 0, 28, &cctx, &mut c, 0, &u_block).unwrap();
    let phys_c = c.extents[0].physical_start;
    assert_ne!(phys_c, 60, "stale entry must not be trusted");
    assert_ne!(phys_c, phys_u, "verify failed -> fresh allocation, not the old share");
    let mut got = [0u8; BLOCK_SIZE];
    ctx.read_block(phys_c, &mut got).unwrap();
    assert!(got.iter().all(|&x| x == b'U'));
}

/// Resets the dedup gate when the test tears down (even on panic).
struct PanicResetDedup;
impl Drop for PanicResetDedup {
    fn drop(&mut self) {
        crate::file::writer::set_dedup_enabled(false);
    }
}
