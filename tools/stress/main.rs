use lionfs_core::btree::tree::BTree;
use lionfs_core::disk::block_io::Disk;
use lionfs_core::inode::tree::INODE_TREE_NODE_TYPE;
use lionfs_core::ondisk::serialization::{Extent, Inode, Superblock, MAX_INLINE_EXTENTS};
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;
use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let test_file = "test_stress.img";
    let _ = fs::remove_file(test_file);
    let disk = Arc::new(Disk::create(test_file, 1024 * 1024 * 200).unwrap()); // 200MB

    let sb = Superblock {
        magic: 0,
        version: 0,
        block_size: 4096,
        total_blocks: 51200,
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
    disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();

    let tm = Arc::new(TransactionManager::new(&sb));

    // Initialize the inode tree using a single thread first
    {
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&disk, &mut tx);
        BTree::<u64, Inode>::init_empty(&mut ctx, 12, INODE_TREE_NODE_TYPE).unwrap();
        tm.commit(&disk, &sb, &tx).unwrap();
    }

    // Use half of available threads, or at least 2
    let available_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let num_threads = std::cmp::max(2, available_threads / 2);
    let ops_per_thread = 500; // Reduced from 2000 to save memory and I/O time

    let mut handles = vec![];
    let start = Instant::now();

    println!(
        "Starting LionFS stress test with {} threads, {} ops/thread...",
        num_threads, ops_per_thread
    );

    for t_id in 0..num_threads {
        let disk_clone = Arc::clone(&disk);
        let tm_clone = Arc::clone(&tm);
        let sb_clone = sb.clone();

        let handle = thread::spawn(move || {
            for i in 0..ops_per_thread {
                let ino = (t_id * 100_000 + i) as u64;

                // Write transaction
                let mut tx = tm_clone.begin(ino);
                {
                    let mut ctx = TxContext::new(&disk_clone, &mut tx);
                    let mut tree = BTree::<u64, Inode>::new(12, INODE_TREE_NODE_TYPE);

                    let mut mock_allocator = |_ctx: &mut TxContext| -> std::io::Result<u64> {
                        // Very naive mock allocator for stress test tree splits
                        Ok(20 + (ino % 1000) * 10)
                    };

                    let inode = Inode {
                        ino,
                        mode: 0,
                        uid: 0,
                        gid: 0,
                        links_count: 0,
                        flags: 0,
                        padding1: 0,
                        size: 1024,
                        ctime: 0,
                        mtime: 0,
                        atime: 0,
                        extent_count: 0,
                        compression_algo: 0,
                        encryption_algo: 0,
                        key_id: 0,
                        extents: [Extent {
                            logical_start: 0,
                            physical_start: 0,
                            length: 0,
                        }; MAX_INLINE_EXTENTS],
                        checksum: 0,
                        spill_pad_head: [0; 4],
                        spill_extent_root: 0,
                    };

                    tree.insert(&mut ctx, ino, inode, &mut mock_allocator)
                        .unwrap();
                }
                tm_clone.commit(&disk_clone, &sb_clone, &tx).unwrap();

                // Read transaction
                let mut rx = tm_clone.begin(ino + 1);
                {
                    let mut ctx = TxContext::new(&disk_clone, &mut rx);
                    let tree = BTree::<u64, Inode>::new(12, INODE_TREE_NODE_TYPE);
                    let found = tree.lookup(&mut ctx, &ino).unwrap().unwrap();
                    assert_eq!(found.ino, ino);
                    assert_eq!(found.size, 1024);
                }

                // Sleep to yield CPU and avoid system lockup
                thread::sleep(Duration::from_millis(2));

                // Yield CPU periodically so we don't hang the user's system
                if i % 10 == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let duration = start.elapsed();
    let total_ops = num_threads * ops_per_thread * 2; // insert + read
    println!("Stress test completed successfully!");
    println!("Total Operations: {}", total_ops);
    println!("Time Elapsed: {:?}", duration);
    println!("Ops/sec: {:.2}", total_ops as f64 / duration.as_secs_f64());

    let _ = fs::remove_file(test_file);
}
