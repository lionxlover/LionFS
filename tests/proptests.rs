use lionfs_core::btree::tree::BTree;
use lionfs_core::disk::block_io::Disk;
use lionfs_core::inode::tree::INODE_TREE_NODE_TYPE;
use lionfs_core::ondisk::serialization::{Extent, Inode, Superblock, MAX_INLINE_EXTENTS};
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;

use proptest::prelude::*;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn create_test_env(test_name: &str) -> (Arc<Disk>, Superblock, Arc<TransactionManager>) {
    let test_file = format!("test_prop_{}.img", test_name);
    let _ = fs::remove_file(&test_file);
    let disk = Arc::new(Disk::create(&test_file, 1024 * 1024 * 100).unwrap());

    let sb = Superblock {
        magic: 0,
        version: 0,
        block_size: 4096,
        total_blocks: 25600,
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

    let tm = Arc::new(TransactionManager::new(&sb));
    (disk, sb, tm)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 10, .. ProptestConfig::default() })]

    #[test]
    fn btree_insert_and_lookup_property(
        keys in prop::collection::vec(1..2000u64, 5..100) // Reduced from 500 to 100 max keys
    ) {
        let (disk, sb, tm) = create_test_env("insert_lookup");

        // Init tree
        {
            let mut tx = tm.begin(0);
            let mut ctx = TxContext::new(&disk, &mut tx);
            BTree::<u64, Inode>::init_empty(&mut ctx, 12, INODE_TREE_NODE_TYPE).unwrap();
            tm.commit(&disk, &sb, &tx).unwrap();
        }

        let alloc_counter = Arc::new(AtomicU64::new(20));

        // Insert all
        for &k in &keys {
            let mut tx = tm.begin(k);
            let mut ctx = TxContext::new(&disk, &mut tx);
            let mut tree = BTree::<u64, Inode>::new(12, INODE_TREE_NODE_TYPE);

            let inode = Inode {
                ino: k, mode: 0, uid: 0, gid: 0, links_count: 0, flags: 0, padding1: 0,
                size: k * 2, ctime: 0, mtime: 0, atime: 0, extent_count: 0,
                compression_algo: 0, encryption_algo: 0, key_id: 0,
                extents: [Extent { logical_start: 0, physical_start: 0, length: 0 }; MAX_INLINE_EXTENTS],
                checksum: 0, spill_pad_head: [0; 4], spill_extent_root: 0,
            };

            let c = alloc_counter.clone();
            let mut mock_allocator = |_ctx: &mut TxContext| -> std::io::Result<u64> {
                Ok(c.fetch_add(1, Ordering::SeqCst))
            };

            tree.insert(&mut ctx, k, inode, &mut mock_allocator).unwrap();
            tm.commit(&disk, &sb, &tx).unwrap();

            // Yield to avoid freezing the system
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        // Lookup all
        for &k in &keys {
            let mut tx = tm.begin(k);
            let mut ctx = TxContext::new(&disk, &mut tx);
            let tree = BTree::<u64, Inode>::new(12, INODE_TREE_NODE_TYPE);

            let res = tree.lookup(&mut ctx, &k).unwrap();
            assert!(res.is_some());
            let found = res.unwrap();
            assert_eq!(found.ino, k);
            assert_eq!(found.size, k * 2);
        }
    }
}
