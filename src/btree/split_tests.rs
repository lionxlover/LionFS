//! Regression tests for BTree split correctness (Phase 0 bug hunt).
//!
//! The pre-existing tests insert keys in increasing order only, which
//! hides two real bugs:
//!
//! 1. **Internal-node split drops the newly-inserted separator**: the
//!    split branch updates `item_count` but never writes the merged
//!    item array back into the parent's payload, so when the new key
//!    lands before the split point the parent keeps stale routing data
//!    and a subtree becomes unreachable ("orphaned leaf").
//! 2. **Root split migration**: when the root splits, a *new* root
//!    block is allocated and only the in-memory `BTree.root_block`
//!    field is updated. Any caller that reconstructs the tree from the
//!    original root block number (which is how every persistent tree
//!    in LionFS is re-opened) finds only the old left half.
//!
//! These tests use shuffled key orders and tree re-open patterns that
//! exercise both paths.

use crate::btree::tree::BTree;
use crate::disk::block_io::Disk;
use crate::ondisk::serialization::Superblock;
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TestKey(pub u64);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TestValue(pub u64);

fn setup(_name: &str) -> (Disk, TransactionManager, Superblock, String) {
    let path = format!("/tmp/lfs_btree_bug_{}.img", std::process::id());
    let _ = std::fs::remove_file(&path);
    let disk = Disk::create(&path, 1024 * 1024 * 512).unwrap();
    let sb = Superblock {
        magic: 0,
        version: 0,
        block_size: 4096,
        total_blocks: 16384,
        free_blocks: 0,
        inode_count: 0,
        root_inode: 0,
        flags: 0,
        padding1: 0,
        bitmap_start: 0,
        inode_table_start: 0,
        data_region_start: 0,
        generation: 0,
        checksum: 0,
        padding_csum: 0,
        journal_start: 1,
        journal_blocks: 10,
        secondary_sb_1: 0,
        secondary_sb_2: 0,
        block_group_count: 0,
        blocks_per_group: 0,
        inode_tree_root: 12,
        dir_tree_root: 0,
        extent_tree_root: 0,
        freespace_tree_root: 0,
        next_ino: 2,
        checksum_tree_root: 0,
        bad_blocks_root: 0,
        crypto_tree_root: 0,
        snapshot_tree_root: 0,
        clone_tree_root: 0,
        refcount_tree_root: 0,
        subvolume_tree_root: 0,
        space_map_root: 0,
        last_snapshot_generation: 0,
        dedupe_tree_root: 0,
        key_tree_root: 0,
        fs_features: 0,
        default_compression: 0,
        default_encryption: 0,
        padding_phase7: [0; 6],
        device_tree_root: 0,
        pool_uuid: [0; 16],
        raid_profile: 0,
        padding_raid: [0; 3],
        chunk_size: 0,
        padding2: [0; 3784],
    };
    disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
    let tm = TransactionManager::new(&sb);
    (disk, tm, sb, path)
}

/// Deterministic xorshift shuffle so failures are reproducible.
fn shuffled_keys(n: u64, seed: u64) -> Vec<u64> {
    let mut keys: Vec<u64> = (0..n).collect();
    let mut state = seed;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for i in (1..keys.len()).rev() {
        let j = (next() as usize) % (i + 1);
        keys.swap(i, j);
    }
    keys
}

#[test]
fn shuffled_inserts_all_findable() {
    let (disk, tm, _sb, path) = setup("shuffled");
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);

    let root = 100u64;
    BTree::<TestKey, TestValue>::init_empty(&mut ctx, root, 1).unwrap();
    let mut btree = BTree::<TestKey, TestValue>::new(root, 1);

    let mut next_block = 101u64;
    let mut allocator = |_: &mut TxContext| {
        let b = next_block;
        next_block += 1;
        Ok(b)
    };

    // Enough items to force *internal-node* splits: a leaf holds ~252
    // items and an internal node routes ~251 children, so the first
    // internal split needs > ~63k items. 70k guarantees several.
    let keys = shuffled_keys(70_000, 0xdeadbeef);
    for &k in &keys {
        btree
            .insert(
                &mut ctx,
                TestKey(k),
                TestValue(k.wrapping_mul(7).wrapping_add(1)),
                &mut allocator,
            )
            .unwrap();
    }

    // Every inserted key must be findable *through the tree we hold*.
    for &k in &keys {
        let v = btree.lookup(&mut ctx, &TestKey(k)).unwrap();
        assert_eq!(
            v,
            Some(TestValue(k.wrapping_mul(7).wrapping_add(1))),
            "key {} lost (in-memory tree)",
            k
        );
    }

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn root_block_stays_stable_across_splits() {
    let (disk, tm, _sb, path) = setup("rootstable");
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);

    let root = 100u64;
    BTree::<TestKey, TestValue>::init_empty(&mut ctx, root, 1).unwrap();
    let mut btree = BTree::<TestKey, TestValue>::new(root, 1);

    let mut next_block = 101u64;
    let mut allocator = |_: &mut TxContext| {
        let b = next_block;
        next_block += 1;
        Ok(b)
    };

    // Sequential is fine here; the point is forcing several root
    // splits. A leaf holds ~252 items, so 20k items force the tree to
    // grow several levels. With the stable-root fix the root block
    // NUMBER must not change; the tree instead grows taller in place.
    for i in 0..20000u64 {
        btree
            .insert(&mut ctx, TestKey(i), TestValue(i), &mut allocator)
            .unwrap();
    }
    assert_eq!(
        btree.root_block, root,
        "root block number must stay stable across splits"
    );

    // The tree must actually have split (otherwise this test would pass
    // trivially): 20k items can't fit in one leaf.
    assert_eq!(
        btree.iter_all(&mut ctx).unwrap().len(),
        20000,
        "all items present via full iteration"
    );

    // A caller that re-opens the tree from the ORIGINAL root block
    // number -- exactly what every persistent tree in LionFS does via
    // superblock fields -- must still see all data.
    let reopened = BTree::<TestKey, TestValue>::new(root, 1);
    for i in 0..20000u64 {
        let v = reopened.lookup(&mut ctx, &TestKey(i)).unwrap();
        assert_eq!(
            v,
            Some(TestValue(i)),
            "key {} lost after re-open from original root {}",
            i,
            root
        );
    }

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn interleaved_inserts_forward_and_reverse() {
    let (disk, tm, _sb, path) = setup("interleaved");
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);

    let root = 100u64;
    BTree::<TestKey, TestValue>::init_empty(&mut ctx, root, 1).unwrap();
    let mut btree = BTree::<TestKey, TestValue>::new(root, 1);

    let mut next_block = 101u64;
    let mut allocator = |_: &mut TxContext| {
        let b = next_block;
        next_block += 1;
        Ok(b)
    };

    // Descending inserts land *before* the split midpoint -- the exact
    // ordering that made the stale-payload bug corrupt routing.
    for i in (0..70_000u64).rev() {
        btree
            .insert(&mut ctx, TestKey(i), TestValue(i + 100), &mut allocator)
            .unwrap();
    }
    for i in 0..70_000u64 {
        let v = btree.lookup(&mut ctx, &TestKey(i)).unwrap();
        assert_eq!(
            v,
            Some(TestValue(i + 100)),
            "key {} lost after reverse inserts",
            i
        );
    }

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}
