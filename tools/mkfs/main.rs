#![allow(clippy::manual_div_ceil, clippy::unnecessary_cast)]

use lionfs_core::btree::tree::BTree;
use lionfs_core::common::uuid::Uuid;
use lionfs_core::disk::block_io::Disk;
use lionfs_core::inode::tree::INODE_TREE_NODE_TYPE;
use lionfs_core::ondisk::serialization::{
    Inode, Superblock, BLOCK_SIZE, LIONFS_MAGIC, MAX_INLINE_EXTENTS,
};
use lionfs_core::pool::raid::{RaidEngine, RaidProfile};
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;

fn parse_raid_profile(s: &str) -> RaidProfile {
    match s.to_lowercase().as_str() {
        "raid0" | "0" => RaidProfile::Raid0,
        "raid1" | "1" => RaidProfile::Raid1,
        "raid5" | "5" => RaidProfile::Raid5,
        "raid6" | "6" => RaidProfile::Raid6,
        "raid10" | "10" => RaidProfile::Raid10,
        _ => RaidProfile::Single,
    }
}

/// Usable logical capacity for a RAID profile, rounded *down* to a whole
/// number of complete stripe rows. This is deliberately conservative
/// (rather than the tightest possible bound) so that no logical block
/// number, once run through `RaidEngine::layout`, can ever produce a
/// physical block offset past what was actually allocated on a device --
/// getting the tightest exact bound right for every profile's integer
/// division edge cases isn't worth the risk of an off-by-one without a way
/// to compile and test it here.
fn usable_blocks(
    per_device_blocks: u64,
    profile: RaidProfile,
    num_devices: usize,
    chunk_size_blocks: u32,
) -> u64 {
    let chunk = chunk_size_blocks.max(1) as u64;
    let raw = match profile {
        RaidProfile::Single => return per_device_blocks, // no striping, no rounding needed
        RaidProfile::Raid0 => per_device_blocks * num_devices as u64,
        RaidProfile::Raid1 => per_device_blocks,
        RaidProfile::Raid5 => per_device_blocks * (num_devices as u64 - 1),
        RaidProfile::Raid6 => per_device_blocks * (num_devices as u64 - 2),
        RaidProfile::Raid10 => per_device_blocks * (num_devices as u64 / 2),
    };
    // Reserve a full extra stripe row of headroom, then round down to a
    // whole number of rows of width `chunk`. Comfortably conservative.
    let row_width = chunk
        * match profile {
            RaidProfile::Raid0 => num_devices as u64,
            RaidProfile::Raid5 => num_devices as u64 - 1,
            RaidProfile::Raid6 => num_devices as u64 - 2,
            RaidProfile::Raid10 => (num_devices as u64 / 2).max(1),
            RaidProfile::Raid1 | RaidProfile::Single => 1,
        };
    let with_margin = raw.saturating_sub(row_width);
    (with_margin / row_width) * row_width
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: mkfs_lfs <image_file> <size_in_mb_per_device> [--raid <single|raid0|raid1|raid5|raid6|raid10> <device2> [device3] ...]");
        std::process::exit(1);
    }

    let image_file = &args[1];
    let size_mb: u64 = args[2].parse().expect("Invalid size");
    // Phase 4: --compress enables zstd compression clusters for all
    // newly created files (per-inode property set at creation from the
    // superblock default).
    let compress = args.iter().any(|a| a == "--compress");

    // Optional multi-device / RAID setup. Backward compatible: with no
    // trailing --raid flag, this behaves exactly as the original
    // single-device-only mkfs did.
    let mut device_paths: Vec<String> = vec![image_file.clone()];
    let mut raid_profile = RaidProfile::Single;
    let mut chunk_size_blocks: u32 = 0;
    if args.len() > 3 {
        if args[3] != "--raid" || args.len() < 5 {
            eprintln!("Usage: mkfs_lfs <image_file> <size_in_mb_per_device> [--compress] [--raid <profile> <device2> [device3] ...] [--chunk <blocks>]");
            std::process::exit(1);
        }
        raid_profile = parse_raid_profile(&args[4]);
        // Optional: --chunk <blocks> anywhere after the profile. Device
        // paths are everything else.
        let mut explicit_chunk: Option<u32> = None;
        let mut rest: Vec<String> = Vec::new();
        let mut it = args[5..].iter();
        while let Some(a) = it.next() {
            if a == "--chunk" {
                let v = it.next().unwrap_or_else(|| {
                    eprintln!("--chunk needs a value (blocks)");
                    std::process::exit(1);
                });
                explicit_chunk = Some(v.parse().unwrap_or_else(|_| {
                    eprintln!("Invalid --chunk value: {}", v);
                    std::process::exit(1);
                }));
            } else if a != "--compress" {
                rest.push(a.clone());
            }
        }
        device_paths.extend(rest);
        // Chunk size rationale (Phase 2): default to 128 KiB chunks
        // (32 blocks at 4 KiB), sector-aligned from the first device's
        // probed geometry when available. Explicit --chunk overrides.
        // This replaces the old bare hardcoded 8 (32 KiB).
        let first_dev_sector = std::fs::OpenOptions::new()
            .read(true)
            .open(&device_paths[0])
            .ok()
            .and_then(|f| lionfs_core::disk::geometry::probe(&f).ok())
            .map(|g| g.logical_sector_size)
            .unwrap_or(0);
        chunk_size_blocks = explicit_chunk
            .unwrap_or_else(|| RaidEngine::recommended_chunk_size_blocks(first_dev_sector));
        println!("RAID chunk size: {} blocks ({} KiB), sector-validated against {}-byte sectors (best-effort default: 128 KiB; override with --chunk)",
            chunk_size_blocks, chunk_size_blocks * (BLOCK_SIZE as u32) / 1024, if first_dev_sector == 0 { 0 } else { first_dev_sector });
        if device_paths.len() < raid_profile.min_devices() {
            eprintln!(
                "RAID profile {:?} needs at least {} devices, got {}",
                raid_profile,
                raid_profile.min_devices(),
                device_paths.len()
            );
            std::process::exit(1);
        }
    }

    let per_device_blocks = (size_mb * 1024 * 1024) / BLOCK_SIZE as u64;
    let total_blocks = usable_blocks(
        per_device_blocks,
        raid_profile,
        device_paths.len(),
        chunk_size_blocks,
    );

    if total_blocks < 10 {
        eprintln!("Size too small");
        std::process::exit(1);
    }

    // Calculate layout
    let bitmap_blocks = (total_blocks + (BLOCK_SIZE as u64 * 8) - 1) / (BLOCK_SIZE as u64 * 8);
    let inode_count: u64 = 1024; // Fixed for now
    let inodes_per_block = BLOCK_SIZE as u64 / std::mem::size_of::<Inode>() as u64;
    let inode_blocks = (inode_count + inodes_per_block - 1) / inodes_per_block;

    let bitmap_start = 1;
    let inode_table_start = bitmap_start + bitmap_blocks;
    let data_region_start = inode_table_start + inode_blocks;

    println!(
        "Formatting {} with {} device(s), {:?}, size {}MB/device ({} usable blocks)",
        image_file,
        device_paths.len(),
        raid_profile,
        size_mb,
        total_blocks
    );

    let disk = Disk::create_pool(
        &device_paths,
        size_mb * 1024 * 1024,
        raid_profile,
        chunk_size_blocks,
    )
    .expect("failed to create device(s)");

    let secondary_sb_1 = if total_blocks > 8192 { 8192 } else { 0 };
    let secondary_sb_2 = if total_blocks > 16384 { 16384 } else { 0 };
    let journal_start = data_region_start;
    let journal_blocks = 4096; // 16 MB flat journal for simplicity

    // Check if image is big enough for this layout
    if total_blocks < journal_start + journal_blocks + 100 {
        panic!(
            "Disk image is too small for Phase 2 layout (requires at least {} blocks)",
            journal_start + journal_blocks + 100
        );
    }

    // Fixed, small block numbers reserved for BTree roots, all comfortably
    // inside the 0..journal_start metadata region (which is at least ~66
    // blocks for any filesystem that passes the size check above,
    // regardless of total_blocks, since inode_count is fixed at 1024).
    let inode_tree_root = 12;
    let checksum_tree_root_blk = 13;
    let bad_blocks_root_blk = 14;
    let key_tree_root_blk = 15;
    let crypto_tree_root_blk = 16;
    let dedupe_tree_root_blk = 17;

    let pool_uuid = Uuid::new_v4().unwrap_or(Uuid::nil());

    let mut sb = Superblock {
        magic: LIONFS_MAGIC,
        version: lionfs_core::common::version::CURRENT_VERSION,
        block_size: BLOCK_SIZE as u32,
        total_blocks,
        free_blocks: total_blocks - (journal_start + journal_blocks), // Subtract metadata and journal
        inode_count,
        root_inode: 1,
        flags: 0,
        padding1: 0,
        bitmap_start: 1,
        inode_table_start,
        data_region_start: journal_start + journal_blocks, // Data region now starts AFTER journal
        generation: 1,
        checksum: 0,
        padding_csum: 0,
        journal_start,
        journal_blocks,
        secondary_sb_1: 8192,
        secondary_sb_2: 16384,
        block_group_count: 1,
        blocks_per_group: total_blocks as u32,
        inode_tree_root,
        dir_tree_root: 0,
        extent_tree_root: 0,
        freespace_tree_root: 0,
        next_ino: 2,
        checksum_tree_root: checksum_tree_root_blk,
        bad_blocks_root: bad_blocks_root_blk,
        snapshot_tree_root: 0,
        clone_tree_root: 0,
        refcount_tree_root: 0,
        subvolume_tree_root: 0,
        space_map_root: 0,
        last_snapshot_generation: 0,
        dedupe_tree_root: dedupe_tree_root_blk,
        key_tree_root: key_tree_root_blk,
        fs_features: 0,
        default_compression: if compress {
            lionfs_core::common::constants::COMPRESSION_ZSTD
        } else {
            0
        },
        default_encryption: 0,
        padding_phase7: [0; 6],
        device_tree_root: 0,
        pool_uuid: *pool_uuid.as_bytes(),
        raid_profile: raid_profile as u8,
        padding_raid: [0; 3],
        chunk_size: chunk_size_blocks,
        crypto_tree_root: crypto_tree_root_blk,
        padding2: [0; BLOCK_SIZE - 312],
    };

    // Calculate checksum
    use lionfs_core::utils::checksum::calculate_superblock_checksum;
    sb.checksum = calculate_superblock_checksum(&sb);

    if compress {
        println!("Compression: zstd clusters (128 KiB units, default level 3; mount with -o zstd_level=N to change)");
    }
    println!(
        "Writing primary superblock (format version {})...",
        sb.version
    );
    disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();

    if secondary_sb_1 != 0 {
        println!("Writing secondary superblock 1...");
        disk.write_block(secondary_sb_1, bytemuck::bytes_of(&sb))
            .unwrap();
    }

    if secondary_sb_2 != 0 {
        println!("Writing secondary superblock 2...");
        disk.write_block(secondary_sb_2, bytemuck::bytes_of(&sb))
            .unwrap();
    }

    // Init bitmap
    let mut bitmap_buf = [0u8; BLOCK_SIZE];
    // Mark blocks 0..data_region_start as used
    // This includes Superblock, Bitmaps, Inodes, and the Journal!
    for i in 0..sb.data_region_start {
        let byte_idx = (i / 8) as usize;
        let bit_idx = i % 8;
        bitmap_buf[byte_idx] |= 1 << bit_idx;
    }
    disk.write_block(bitmap_start, &bitmap_buf).unwrap();
    for i in 1..bitmap_blocks {
        disk.write_block(bitmap_start + i, &[0; BLOCK_SIZE])
            .unwrap();
    }

    // Init inodes
    for i in 0..inode_blocks {
        disk.write_block(inode_table_start + i, &[0; BLOCK_SIZE])
            .unwrap();
    }

    // Create Root Inode (ino 1)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let root_inode = Inode {
        ino: 1,
        mode: lionfs_core::pal::posix::S_IFDIR | 0o755,
        uid: 1000,
        gid: 1000,
        links_count: 2,
        flags: 0,
        padding1: 0,
        size: 0,
        ctime: now,
        mtime: now,
        atime: now,
        extent_count: 0,
        compression_algo: 0,
        encryption_algo: 0,
        key_id: 0,
        extents: [lionfs_core::ondisk::serialization::Extent {
            logical_start: 0,
            physical_start: 0,
            length: 0,
        }; MAX_INLINE_EXTENTS],
        checksum: 0,
        spill_pad_head: [0; 4],
        spill_extent_root: 0,
    };

    // Use proper BTree to store the root inode
    let tm = TransactionManager::new(&sb);
    let mut tx = tm.begin(0);
    {
        let mut ctx = TxContext::new(&disk, &mut tx);
        BTree::<u64, Inode>::init_empty(&mut ctx, sb.inode_tree_root, INODE_TREE_NODE_TYPE)
            .unwrap();
        let mut tree = BTree::<u64, Inode>::new(sb.inode_tree_root, INODE_TREE_NODE_TYPE);

        let mut mock_allocator = |_ctx: &mut TxContext| -> std::io::Result<u64> { Ok(20) };
        tree.insert(&mut ctx, 1, root_inode, &mut mock_allocator)
            .unwrap();

        // Explicitly initialize the remaining always-present metadata
        // trees rather than relying on sparse-file zero-fill to look like
        // a valid empty leaf node -- correct either way, but explicit is
        // more robust and self-documenting.
        lionfs_core::integrity::checksum_tree::ChecksumTree::init_empty(
            &mut ctx,
            sb.checksum_tree_root,
        )
        .unwrap();
        lionfs_core::integrity::bad_blocks::BadBlockManager::init_empty(
            &mut ctx,
            sb.bad_blocks_root,
        )
        .unwrap();
        lionfs_core::security::keys::KeyTree::init_empty(&mut ctx, sb.key_tree_root).unwrap();
        lionfs_core::security::block_cipher::BlockTransformTree::init_empty(
            &mut ctx,
            sb.crypto_tree_root,
        )
        .unwrap();
        lionfs_core::fs::dedupe::DedupeTree::init_empty(&mut ctx, sb.dedupe_tree_root).unwrap();
    }
    tm.commit(&disk, &sb, &tx).unwrap();

    disk.sync().unwrap();
    println!("Format complete! Pool UUID: {}", pool_uuid);
}
