//! `lfs_snapshot` -- real snapshot lifecycle on a LionFS image.
//!
//! 3.3: this was a placeholder that printed "created successfully"
//! without touching the device -- exactly the class of dishonest stub
//! this project removed everywhere else. It now opens the image,
//! runs the real SnapshotManager (O(1)-metadata creation: records the
//! current frozen-tree roots + pins data extents), commits through
//! the journal, and persists the superblock.
//!
//! `verify` demonstrates the capability that metadata CoW unlocked:
//! reading a snapshot's data and checking it against the snapshot's
//! OWN frozen checksum view (3.2 snapshot reads had to disable
//! verification because the checksum tree was shared and mutable).

use lionfs_core::disk::block_io::Disk;
use lionfs_core::fs::snapshots::SnapshotManager;
use lionfs_core::integrity::algorithms::{calculate_checksum, ChecksumAlgorithm};
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
use lionfs_core::ondisk::superblock as sbio;
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;

fn read_sb(disk: &Disk) -> Superblock {
    let mut buf = [0u8; BLOCK_SIZE];
    disk.read_block(0, &mut buf).expect("Failed to read superblock");
    let sb: Superblock = *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
    if sb.magic != LIONFS_MAGIC {
        eprintln!("ERROR: not a LionFS image (bad magic)");
        std::process::exit(1);
    }
    sb
}

fn bg_of(sb: &Superblock) -> lionfs_core::ondisk::serialization::BlockGroupDescriptor {
    lionfs_core::ondisk::serialization::BlockGroupDescriptor {
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

fn finish(mut disk: &mut Disk, mut sb: Superblock, tx_id: u64) {
    // Persist the (possibly root-moved / barrier-changed) superblock,
    // including the node-stamp high-water mark so the next mount
    // restarts stamps above everything this process wrote.
    sb.node_generation = lionfs_core::btree::tree::node_gen_current();
    if let Err(e) = sbio::write_all_slots(&mut disk, &sb, tx_id) {
        eprintln!("warning: superblock persist failed: {e}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: lfs_snapshot <create|delete|list|verify> <image> [args]");
        eprintln!("  create <image> <snapshot_id>          -- pin data + freeze tree roots");
        eprintln!("  delete <image> <snapshot_id>          -- release pins, recompute barrier");
        eprintln!("  list   <image>                        -- every live snapshot");
        eprintln!("  verify <image> <snapshot_id> <ino>    -- check snapshot data against its");
        eprintln!("                                            frozen checksum view (3.3)");
        std::process::exit(1);
    }
    let cmd = args[1].as_str();
    let image = &args[2];

    let mut disk = match Disk::open(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: cannot open {image}: {e}");
            std::process::exit(1);
        }
    };
    let mut sb = read_sb(&disk);
    // Phase 9: mirror the persisted barrier so any mutation below is
    // CoW-aware even though this tool builds a bare context.
    disk.live_barrier
        .fetch_max(sb.last_snapshot_generation, std::sync::atomic::Ordering::AcqRel);

    match cmd {
        "create" => {
            let id: u64 = args
                .get(3)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    eprintln!("ERROR: create needs a numeric snapshot id");
                    std::process::exit(1);
                });
            if sb.snapshot_tree_root == 0 {
                eprintln!("ERROR: image has no snapshot tree (mkfs did not initialize one)");
                std::process::exit(1);
            }
            let tm = TransactionManager::new(&sb);
            let mut tx = tm.begin(0);
            let bg = bg_of(&sb);
            let bpg = sb.blocks_per_group;
            let mut snap = SnapshotManager::new(sb.snapshot_tree_root);
            let tx_id = tx.id;
            let mut ctx = TxContext::new(&disk, &mut tx);
            let result = snap.create_snapshot(
                &mut ctx,
                &mut sb,
                id,
                0,
                &mut |c| {
                    lionfs_core::allocator::bitmap::Allocator::allocate_extents_meta(
                        c, &bg, bpg, 1,
                    )
                },
            );
            drop(ctx);
            if let Err(e) = result {
                eprintln!("ERROR: create failed: {e}");
                std::process::exit(1);
            }
            let _ = tm.commit(&disk, &sb, &tx);
            finish(&mut disk, sb, tx_id);
            println!(
                "snapshot {id} created (metadata O(1); data extent runs pinned; barrier gen {})",
                sb.last_snapshot_generation
            );
        }
        "delete" => {
            let id: u64 = args
                .get(3)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    eprintln!("ERROR: delete needs a numeric snapshot id");
                    std::process::exit(1);
                });
            let tm = TransactionManager::new(&sb);
            let mut tx = tm.begin(0);
            let bg = bg_of(&sb);
            let bpg = sb.blocks_per_group;
            let mut snap = SnapshotManager::new(sb.snapshot_tree_root);
            let tx_id = tx.id;
            let mut ctx = TxContext::new(&disk, &mut tx);
            let result = snap.delete_snapshot(
                &mut ctx,
                &mut sb,
                id,
                &mut |c| {
                    lionfs_core::allocator::bitmap::Allocator::allocate_extents_meta(
                        c, &bg, bpg, 1,
                    )
                },
            );
            drop(ctx);
            if let Err(e) = result {
                eprintln!("ERROR: delete failed: {e}");
                std::process::exit(1);
            }
            let _ = tm.commit(&disk, &sb, &tx);
            finish(&mut disk, sb, tx_id);
            println!("snapshot {id} deleted (pins released; barrier now {})", sb.last_snapshot_generation);
        }
        "list" => {
            if sb.snapshot_tree_root == 0 {
                println!("no snapshot tree on this image");
                return;
            }
            let tm = TransactionManager::new(&sb);
            let mut tx = tm.begin(0);
            let snap = SnapshotManager::new(sb.snapshot_tree_root);
            let mut ctx = TxContext::new(&disk, &mut tx);
            match snap.list_snapshots(&mut ctx) {
                Ok(recs) => {
                    if recs.is_empty() {
                        println!("no snapshots");
                    }
                    println!("ID\tCREATED\t\tGEN\tINODE_ROOT\tCSUM_ROOT");
                    for r in recs {
                        println!(
                            "{}\t{}\t{}\t{}\t\t{}",
                            r.id, r.creation_time, r.generation, r.inode_tree_root, r.checksum_tree_root
                        );
                    }
                }
                Err(e) => {
                    eprintln!("ERROR: list failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        "verify" => {
            let id: u64 = args
                .get(3)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    eprintln!("ERROR: verify needs a snapshot id");
                    std::process::exit(1);
                });
            let ino: u64 = args
                .get(4)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    eprintln!("ERROR: verify needs an inode number");
                    std::process::exit(1);
                });
            let tm = TransactionManager::new(&sb);
            let mut tx = tm.begin(0);
            let snap = SnapshotManager::new(sb.snapshot_tree_root);
            let mut ctx = TxContext::new(&disk, &mut tx);

            let inode = match snap.read_snapshot_inode(&mut ctx, id, ino) {
                Ok(Some(i)) => i,
                Ok(None) => {
                    eprintln!("inode {ino} is not in snapshot {id}'s view");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            };
            if inode.compression_algo != 0 {
                eprintln!("compressed inodes are outside snapshot pin coverage (documented 3.3 limitation)");
                std::process::exit(1);
            }
            let mut checked = 0u64;
            let mut mismatches = 0u64;
            let mut missing = 0u64;
            for slot in inode.extents.iter().take(inode.extent_count as usize) {
                for b in 0..slot.length {
                    let lb = slot.logical_start + b;
                    let phys = slot.physical_start + b;
                    let mut buf = [0u8; BLOCK_SIZE];
                    if ctx.read_block(phys, &mut buf).is_err() {
                        missing += 1;
                        continue;
                    }
                    match snap.read_snapshot_csum(&mut ctx, id, ino, lb) {
                        Ok(Some(v)) => {
                            let recomputed =
                                calculate_checksum(ChecksumAlgorithm::XxHash64, &buf);
                            if recomputed[..] != v.checksum_bytes[..] {
                                mismatches += 1;
                                eprintln!("MISMATCH inode {ino} logical block {lb}");
                            } else {
                                checked += 1;
                            }
                        }
                        Ok(None) => {
                            missing += 1;
                        }
                        Err(e) => {
                            eprintln!("csum lookup error at {lb}: {e}");
                            missing += 1;
                        }
                    }
                }
            }
            println!(
                "snapshot {id} / inode {ino}: {} blocks verified OK, {mismatches} mismatches, {missing} without csum records",
                checked
            );
            if mismatches > 0 {
                std::process::exit(2);
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(1);
        }
    }
}
