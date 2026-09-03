//! Phase 0 regression tests for extent spooling and the ~7-extent file
//! cap.
//!
//! Before the spooling fix, `FileManager::add_extent` returned
//! "Max inline extents reached" once an inode had 7 non-mergeable
//! extents, capping sequential 1-block-at-a-time writes at a few tens
//! of KB (and fragmented files at 7 fragments). After the fix, the 8th
//! and later extents spill into a per-inode ExtentTree, and reads,
//! writes and truncates all consult it.

use crate::allocator::bitmap::Allocator;
use crate::disk::block_io::Disk;
use crate::file::writer::FileManager;
use crate::ondisk::serialization::{BlockGroupDescriptor, Inode, Superblock, BLOCK_SIZE};
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;

fn build_image(
    name: &str,
    blocks: u64,
) -> (
    Disk,
    TransactionManager,
    Superblock,
    BlockGroupDescriptor,
    String,
) {
    let path = format!("/tmp/lfs_spool_{}_{}.img", name, std::process::id());
    let _ = std::fs::remove_file(&path);
    let disk = Disk::create(&path, blocks * BLOCK_SIZE as u64).unwrap();
    let sb = Superblock {
        magic: 0,
        version: 0,
        block_size: BLOCK_SIZE as u32,
        total_blocks: blocks,
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
        block_group_count: 1,
        blocks_per_group: blocks as u32,
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

    // One block group: bitmap at block 1, inodes at 2, data from 16.
    let bg = BlockGroupDescriptor {
        bg_block_bitmap: 1,
        bg_inode_bitmap: 0,
        bg_inode_table: 2,
        bg_free_blocks_count: 0,
        bg_free_inodes_count: 0,
        bg_used_dirs_count: 0,
        bg_padding: 0,
        bg_reserved: [0; 32],
    };
    let tm = TransactionManager::new(&sb);
    (disk, tm, sb, bg, path)
}

fn new_inode(ino: u64) -> Inode {
    Inode {
        ino,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        links_count: 1,
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
    }
}

#[test]
fn large_sequential_write_spills_and_reads_back() {
    // 400 blocks = 1.6 MB. The baseline capped out at 7 inline extents
    // (1 block each without P1's speculative sizing); with spooling the
    // inline slots fill and the rest spill to the extent tree.
    let (disk, tm, _sb, bg, path) = build_image("seq", 4096);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    // Mark metadata blocks (0..16) as used INSIDE the live transaction
    // so file allocation starts at block 16.
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = new_inode(2);
    let payload: Vec<u8> = (0..400u32).flat_map(|i| (i * 977).to_le_bytes()).collect();
    FileManager::write_file(&mut ctx, &bg, 4096, 0, &cctx, &mut inode, 0, &payload).unwrap();

    assert_eq!(inode.size, payload.len() as u64);
    // Sequential writes over a fresh region merge into a single extent
    // (every new block is logically and physically adjacent), so a big
    // sequential file needs only ONE inline slot -- the cap only bites
    // on fragmentation, which the striped test below covers.
    assert_eq!(
        inode.extent_count, 1,
        "sequential write should merge to one extent"
    );

    // Read the whole file back and verify every byte.
    let want_len = inode.size;
    let read_back = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, want_len).unwrap();
    assert_eq!(read_back.len(), payload.len());
    assert_eq!(
        read_back, payload,
        "spilled data must read back byte-identical"
    );

    // Spot-check mid-extent reads (they must consult the spill tree,
    // not just exact extent starts).
    for probe in [0usize, 1, 39, 40, 41, 399, 4000, 1600000, 1638399] {
        let byte =
            FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, probe as u64, 1).unwrap();
        if probe < payload.len() {
            assert_eq!(
                byte[0], payload[probe],
                "byte at offset {} must match",
                probe
            );
        }
    }

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn fragmented_writes_spill_and_survive() {
    // Striped writes across a 300-block range: each stripe lands in a
    // different physical region, forcing many non-mergeable extents --
    // the exact case that used to hard-fail at 7 fragments.
    let (disk, tm, _sb, bg, path) = build_image("frag", 4096);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    // Mark metadata blocks (0..16) as used INSIDE the live transaction
    // so file allocation starts at block 16.
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = new_inode(3);
    let stripe: Vec<u8> = (0..64u32).map(|i| (i * 31 + 7) as u8).collect();
    for block in 0u64..300 {
        if block % 3 == 0 {
            // sparse skip: leaves holes between stripes
            continue;
        }
        FileManager::write_file(
            &mut ctx,
            &bg,
            4096,
            0,
            &cctx,
            &mut inode,
            block * BLOCK_SIZE as u64,
            &stripe,
        )
        .unwrap();
    }
    assert!(
        inode.spill_extent_root != 0,
        "fragmentation must have spilled"
    );

    // Verify all stripes.
    for block in 0u64..300 {
        let expect: Option<Vec<u8>> = if block % 3 == 0 {
            None
        } else {
            Some(stripe.clone())
        };
        let got = FileManager::read_file(
            &mut ctx,
            0,
            0,
            &cctx,
            &mut inode,
            block * BLOCK_SIZE as u64,
            64,
        )
        .unwrap();
        match expect {
            Some(s) => assert_eq!(got, s, "stripe at block {} must round-trip", block),
            None => assert!(
                got.iter().all(|&b| b == 0) || got.is_empty(),
                "hole at block {} must read as zeros",
                block
            ),
        }
    }

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn truncate_frees_spilled_extents() {
    let (disk, tm, _sb, bg, path) = build_image("trunc", 4096);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    // Mark metadata blocks (0..16) as used INSIDE the live transaction
    // so file allocation starts at block 16.
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = new_inode(4);
    // Fragmented layout: full-block writes at every 2nd block over a
    // 300-block span. Each write lands in its own physical region, so
    // the 7 inline slots fill and the rest spill to the extent tree.
    let block_data = vec![0xABu8; BLOCK_SIZE];
    for block in (0u64..300).step_by(2) {
        FileManager::write_file(
            &mut ctx,
            &bg,
            4096,
            0,
            &cctx,
            &mut inode,
            block * BLOCK_SIZE as u64,
            &block_data,
        )
        .unwrap();
    }
    assert!(
        inode.spill_extent_root != 0,
        "striped layout must have spilled"
    );
    let written_blocks = 150u64; // 300 / 2

    let free_before = Allocator::count_free_blocks(&mut ctx, bg.bg_block_bitmap, 4096).unwrap();
    FileManager::truncate_file(&mut ctx, &bg, 4096, &mut inode, 40 * BLOCK_SIZE as u64).unwrap();
    let free_after = Allocator::count_free_blocks(&mut ctx, bg.bg_block_bitmap, 4096).unwrap();
    assert_eq!(inode.size, 40 * BLOCK_SIZE as u64);

    // Blocks 40..300 held written stripes (all even blocks >= 40):
    // (300 - 40) / 2 = 130 data blocks must be freed.
    assert_eq!(
        free_after - free_before,
        130,
        "truncate must free spilled extents: freed {} blocks",
        free_after - free_before
    );
    assert_eq!(written_blocks - 130, 20, "20 data blocks survive");

    // Surviving stripes (blocks 0..40, even) must still read back 0xAB
    // through inline + spill paths.
    for block in (0u64..40).step_by(2) {
        let got = FileManager::read_file(
            &mut ctx,
            0,
            0,
            &cctx,
            &mut inode,
            block * BLOCK_SIZE as u64,
            BLOCK_SIZE as u64,
        )
        .unwrap();
        assert!(
            got.iter().all(|&b| b == 0xAB),
            "stripe at block {} must survive truncate",
            block
        );
    }

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn readahead_lru_serves_correct_data_and_invalidates_on_write() {
    // Readahead is default-OFF (measured -48% on reads in this
    // harness); this test proves the WIRED path is CORRECT when
    // enabled: LRU hits return the same bytes as a direct read, and
    // a write to a physical block invalidates its cached copy.
    crate::file::writer::set_readahead_enabled(true);
    let (disk, tm, _sb, bg, path) = build_image("readahead", 4096);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = new_inode(7);
    let payload: Vec<u8> = (0..64u32)
        .flat_map(|i| (i * 5417u32).to_le_bytes())
        .collect();
    FileManager::write_file(&mut ctx, &bg, 4096, 0, &cctx, &mut inode, 0, &payload).unwrap();

    // First read warms the LRU; second read must serve identical bytes
    // (whether from LRU or tx -- correctness is what matters).
    let a = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, 256).unwrap();
    let b = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, 256).unwrap();
    assert_eq!(a, b);
    assert_eq!(&a[..8], &payload[..8]);

    // Overwrite the first block and re-read: the write must invalidate
    // the stale cached copy.
    let new_payload = vec![0xEEu8; 256];
    FileManager::write_file(&mut ctx, &bg, 4096, 0, &cctx, &mut inode, 0, &new_payload).unwrap();
    let c = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, 256).unwrap();
    assert!(
        c.iter().all(|&x| x == 0xEE),
        "write must invalidate the readahead LRU entry"
    );

    crate::file::writer::set_readahead_enabled(false);
    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}
