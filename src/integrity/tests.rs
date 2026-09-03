#[cfg(test)]
mod tests {
    use crate::btree::tree::BTree;
    use crate::disk::block_io::Disk;
    use crate::inode::tree::INODE_TREE_NODE_TYPE;
    use crate::ondisk::serialization::Inode;
    use crate::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
    use crate::transaction::manager::TransactionManager;
    use crate::transaction::transaction::TxContext;
    use std::fs;

    #[test]
    fn test_btree_node_corruption_detection() {
        let test_file = "test_corruption.img";
        let _ = fs::remove_file(test_file);

        let mut disk = Disk::create(test_file, 10 * BLOCK_SIZE as u64).unwrap();
        let sb = Superblock {
            magic: LIONFS_MAGIC,
            version: 1,
            block_size: BLOCK_SIZE as u32,
            total_blocks: 10,
            free_blocks: 5,
            inode_count: 10,
            root_inode: 1,
            flags: 0,
            padding1: 0,
            bitmap_start: 1,
            inode_table_start: 2,
            data_region_start: 3,
            generation: 1,
            checksum: 0,
            padding_csum: 0,
            journal_start: 3,
            journal_blocks: 2,
            secondary_sb_1: 0,
            secondary_sb_2: 0,
            block_group_count: 1,
            blocks_per_group: 10,
            inode_tree_root: 5,
            dir_tree_root: 6,
            extent_tree_root: 7,
            freespace_tree_root: 8,
            next_ino: 2,
            checksum_tree_root: 9,
            bad_blocks_root: 10,
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
            crypto_tree_root: 0,
            padding2: [0; 3784],
        };

        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);

        // 1. Initialize an InodeTree
        BTree::<u64, Inode>::init_empty(&mut ctx, sb.inode_tree_root, INODE_TREE_NODE_TYPE)
            .unwrap();

        // 2. Insert an inode
        let inode = Inode {
            ino: 42,
            mode: 0,
            uid: 0,
            gid: 0,
            links_count: 0,
            flags: 0,
            padding1: 0,
            size: 0,
            ctime: 0,
            mtime: 0,
            atime: 0,
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

        let mut tree = BTree::<u64, Inode>::new(sb.inode_tree_root, INODE_TREE_NODE_TYPE);
        let mut allocate_block = |_ctx: &mut TxContext| -> std::io::Result<u64> {
            Ok(99) // mock allocator
        };
        tree.insert(&mut ctx, 42, inode, &mut allocate_block)
            .unwrap();

        // Extract the modified block 5 from the transaction
        let mut corrupted_buf = ctx
            .tx
            .dirty_blocks
            .get(&sb.inode_tree_root)
            .unwrap()
            .clone();

        // Corrupt the payload area where the inode was inserted
        corrupted_buf[256] ^= 0xFF;

        // Write the corrupted block directly to disk
        disk.write_block(sb.inode_tree_root, &corrupted_buf)
            .unwrap();
        disk.sync().unwrap();

        // 4. Try reading it back with a NEW transaction
        let mut tx2 = tm.begin(1);
        let mut ctx2 = TxContext::new(&mut disk, &mut tx2);

        let tree = BTree::<u64, Inode>::new(sb.inode_tree_root, INODE_TREE_NODE_TYPE);
        let res = tree.lookup(&mut ctx2, &42);

        assert!(
            res.is_err(),
            "Corruption was not detected! Expected an error."
        );
        assert_eq!(res.unwrap_err().to_string(), "BTree Node Checksum Mismatch");

        // Cleanup
        let _ = fs::remove_file(test_file);
    }
}
