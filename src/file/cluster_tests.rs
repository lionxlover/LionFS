//! Phase 4 regression tests: compression clusters actually save space,
//! round-trip, truncate, and fall back to raw for incompressible data.

use crate::allocator::bitmap::Allocator;
use crate::disk::block_io::Disk;
use crate::file::cluster::{ClusterTree, CLUSTER_BLOCKS, CLUSTER_BYTES};
use crate::file::writer::FileManager;
use crate::ondisk::serialization::{BlockGroupDescriptor, Inode, Superblock, BLOCK_SIZE};
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;

/// Mixed-compressibility corpus (the plan explicitly requires this, not
/// artificially-repetitive test data):
///   40% highly compressible (repeating 64-byte records)
///   35% text-like (small dictionary, low entropy)
///   25% incompressible (PRNG bytes)
/// Deterministic so ratios are reproducible.
fn mixed_corpus(len: usize) -> Vec<u8> {
    let mut rng: u64 = 0xC0FFEE;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let dict: [&[u8]; 16] = [
        b"the", b"quick", b"brown", b"fox", b"jumps", b"over", b"lazy", b"dogs", b"lorem",
        b"ipsum", b"dolor", b"sit", b"amet", b"sed", b"do", b"eiusmod",
    ];
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let section = next() % 100;
        let target = (len - out.len()).min(4096);
        let before = out.len();
        if section < 40 {
            let seed = (next() & 0xFF) as u8;
            let pat: Vec<u8> = (0..64u8)
                .map(|i| i.wrapping_mul(31).wrapping_add(seed))
                .collect();
            while out.len() < before + target {
                let take = 64.min(before + target - out.len());
                out.extend_from_slice(&pat[..take]);
            }
        } else if section < 75 {
            for _ in 0..(target / 4) {
                let w = dict[(next() % 16) as usize];
                out.extend_from_slice(w);
                out.push(b' ');
            }
            while out.len() < before + target {
                out.push(b'.');
            }
        } else {
            for _ in 0..target {
                out.push((next() & 0xFF) as u8);
            }
        }
    }
    out.truncate(len);
    out
}

fn build_env(
    tag: &str,
    blocks: u64,
) -> (
    Disk,
    TransactionManager,
    Superblock,
    BlockGroupDescriptor,
    String,
) {
    let path = format!("/tmp/lfs_cluster_{}_{}.img", tag, std::process::id());
    let _ = std::fs::remove_file(&path);
    let disk = Disk::create(&path, blocks * BLOCK_SIZE as u64).unwrap();
    let sb = Superblock {
        magic: 0,
        version: 2,
        block_size: 4096,
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

fn compressed_inode(ino: u64) -> Inode {
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
        compression_algo: 2,
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

fn plain_inode(ino: u64) -> Inode {
    let mut i = compressed_inode(ino);
    i.compression_algo = 0;
    i
}

fn blocks_used(ctx: &mut TxContext, bg: &BlockGroupDescriptor, total: u64) -> u64 {
    total - Allocator::count_free_blocks(ctx, bg.bg_block_bitmap, total).unwrap()
}

#[test]
fn cluster_roundtrip_and_real_space_savings() {
    let (disk, tm, _sb, bg, path) = build_env("roundtrip", 16384);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = compressed_inode(2);
    // 3 clusters of mixed corpus = 384 KiB logical.
    let corpus = mixed_corpus((CLUSTER_BYTES * 3) as usize);

    let before = blocks_used(&mut ctx, &bg, 16384);
    FileManager::write_file(&mut ctx, &bg, 16384, 0, &cctx, &mut inode, 0, &corpus).unwrap();
    let after = blocks_used(&mut ctx, &bg, 16384);
    let consumed = after - before;
    let logical_blocks = (corpus.len() as u64).div_ceil(BLOCK_SIZE as u64);

    assert_eq!(inode.size, corpus.len() as u64);
    assert_eq!(
        inode.extent_count, 0,
        "compressed inode keeps no inline extents"
    );
    assert!(inode.spill_extent_root != 0, "cluster tree must exist");
    assert!(
        consumed < logical_blocks,
        "compression must actually save space: consumed {} blocks for {} logical",
        consumed,
        logical_blocks
    );
    let ratio = logical_blocks as f64 / consumed as f64;
    assert!(
        ratio > 1.3,
        "mixed corpus should compress meaningfully (ratio {:.2})",
        ratio
    );

    // Full read-back must be byte-identical.
    let want_len = inode.size;
    let back = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, want_len).unwrap();
    assert_eq!(back.len(), corpus.len());
    assert_eq!(back, corpus, "cluster round-trip must be byte-identical");

    // Partial reads at awkward offsets (cluster cache + splicing).
    for probe in [0usize, 1, 4096, 131071, 131072, 131073, 262144, 393215 - 97] {
        let got =
            FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, probe as u64, 97).unwrap();
        assert_eq!(
            &got[..],
            &corpus[probe..probe + 97],
            "probe {} must round-trip",
            probe
        );
    }

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn incompressible_data_falls_back_to_raw() {
    let (disk, tm, _sb, bg, path) = build_env("raw", 16384);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = compressed_inode(3);
    // Pure PRNG data: zstd cannot shrink it; clusters must store raw
    // and not grow beyond logical + 1 block of padding per cluster.
    let mut rng: u64 = 0xDEAD_BEEF;
    let random_data: Vec<u8> = (0..CLUSTER_BYTES as usize * 2)
        .map(|_| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng & 0xFF) as u8
        })
        .collect();

    let before = blocks_used(&mut ctx, &bg, 16384);
    FileManager::write_file(&mut ctx, &bg, 16384, 0, &cctx, &mut inode, 0, &random_data).unwrap();
    let after = blocks_used(&mut ctx, &bg, 16384);
    let consumed = after - before;
    let logical_blocks = (random_data.len() as u64).div_ceil(BLOCK_SIZE as u64);

    assert!(
        consumed <= logical_blocks + 2,
        "random data must not grow beyond logical size (consumed {} vs {})",
        consumed,
        logical_blocks
    );

    // Tree entries must be marked raw.
    let tree = ClusterTree::new(inode.spill_extent_root);
    for ci in 0..2u64 {
        let v = tree.get(&mut ctx, ci).unwrap().expect("entry exists");
        assert_eq!(v.is_raw, 1, "cluster {} must be stored raw", ci);
    }

    // Round-trip anyway.
    let want_len = inode.size;
    let back = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, want_len).unwrap();
    assert_eq!(back, random_data);

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cluster_overwrite_and_truncate() {
    let (disk, tm, _sb, bg, path) = build_env("trunc", 16384);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = compressed_inode(4);
    let corpus = mixed_corpus((CLUSTER_BYTES * 3) as usize);
    FileManager::write_file(&mut ctx, &bg, 16384, 0, &cctx, &mut inode, 0, &corpus).unwrap();

    // Overwrite the middle of cluster 1 (a cluster RMW).
    let patch: Vec<u8> = (0..8192).map(|i| (i % 7 + 1) as u8).collect();
    FileManager::write_file(
        &mut ctx,
        &bg,
        16384,
        0,
        &cctx,
        &mut inode,
        CLUSTER_BYTES + 4096,
        &patch,
    )
    .unwrap();
    let mut expected = corpus.clone();
    expected[131072 + 4096..131072 + 4096 + 8192].copy_from_slice(&patch);
    let want_len = inode.size;
    let back = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, want_len).unwrap();
    assert_eq!(back, expected, "partial overwrite must splice correctly");

    // Truncate mid-cluster: frees whole clusters beyond + rewrites the
    // partial one shorter.
    let new_size = CLUSTER_BYTES * 2 + 4096; // keep 2 full + 1 partial (4 KiB)
    let free_before = Allocator::count_free_blocks(&mut ctx, bg.bg_block_bitmap, 16384).unwrap();
    FileManager::truncate_file(&mut ctx, &bg, 16384, &mut inode, new_size).unwrap();
    let free_after = Allocator::count_free_blocks(&mut ctx, bg.bg_block_bitmap, 16384).unwrap();
    assert_eq!(inode.size, new_size);
    assert!(
        free_after > free_before,
        "truncate must free cluster extents"
    );

    let back2 = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, new_size).unwrap();
    assert_eq!(back2.len(), new_size as usize);
    assert_eq!(
        &back2[..],
        &expected[..new_size as usize],
        "truncated prefix must survive"
    );

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn uncompressed_inode_unaffected_by_cluster_code() {
    // A plain inode on the same (v2) filesystem must behave exactly as
    // the pre-cluster code: inline extents, spill tree, per-block
    // checksums.
    let (disk, tm, _sb, bg, path) = build_env("plain", 8192);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = plain_inode(5);
    let data = vec![0x77u8; 200 * BLOCK_SIZE];
    FileManager::write_file(&mut ctx, &bg, 8192, 0, &cctx, &mut inode, 0, &data).unwrap();
    // Phase 1's speculative sizing means a sequential plain write
    // merges into ONE inline extent (no spill tree needed) -- the
    // important property here is that the cluster code did not hijack
    // the plain path.
    assert_eq!(
        inode.extent_count, 1,
        "sequential plain write merges to one extent"
    );
    assert_eq!(
        inode.spill_extent_root, 0,
        "no cluster tree for an uncompressed inode"
    );
    let want_len = inode.size;
    let back = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, want_len).unwrap();
    assert_eq!(back, data);

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cluster_tree_reports_physical_usage() {
    let (disk, tm, _sb, bg, path) = build_env("usage", 16384);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    Allocator::mark_blocks_used(&mut ctx, bg.bg_block_bitmap, 0, 16).unwrap();
    let cctx = BlockCipherContext::none();

    let mut inode = compressed_inode(6);
    let corpus = mixed_corpus((CLUSTER_BYTES * 2) as usize);
    FileManager::write_file(&mut ctx, &bg, 16384, 0, &cctx, &mut inode, 0, &corpus).unwrap();

    let tree = ClusterTree::new(inode.spill_extent_root);
    let used = tree.total_physical_blocks(&mut ctx).unwrap();
    let logical = 2 * CLUSTER_BLOCKS;
    assert!(
        used < logical,
        "tree-reported usage ({}) must be under logical ({})",
        used,
        logical
    );

    drop(ctx);
    drop(tx);
    drop(tm);
    let _ = std::fs::remove_file(&path);
}
