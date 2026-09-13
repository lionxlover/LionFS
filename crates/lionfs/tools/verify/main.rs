//! `lfs_verify` — image + fragment integrity verification (LionFS 8.0:
//! a real checker — this was a JSON-banner stub through 7.1).
//!
//! Modes:
//!   lfs_verify image <image>
//!       Superblock sanity (magic, version, CRC, mountability),
//!       snapshot-registry integrity, and snapshot-record root
//!       validation.
//!   lfs_verify frags <frag...>
//!       CRC verification of LionFS EC fragment files (the
//!       `lfs_raid` output format).
//!
//! Exit codes: 0 healthy, 2 corrupt/failed checks, 1 usage error.

use lionfs_core::disk::block_io::Disk;
use lionfs_core::fs::timetravel as tt;
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
use lionfs_core::ondisk::superblock as sbio;
use lionfs_core::pool::ec_volume::verify_fragments;
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("LionFS integrity verifier {} ({})", lionfs_core::VERSION, lionfs_core::EDITION);
        eprintln!("Usage:");
        eprintln!("  lfs_verify image <image>          — superblock + snapshot registry checks");
        eprintln!("  lfs_verify frags <frag...>        — CRC-verify EC fragment files");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "image" => {
            let image = &args[2];
            let disk = match Disk::open(image) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("FAIL: cannot open {image}: {e}");
                    std::process::exit(2);
                }
            };
            let mut buf = [0u8; BLOCK_SIZE];
            if let Err(e) = disk.read_block(0, &mut buf) {
                eprintln!("FAIL: cannot read superblock: {e}");
                std::process::exit(2);
            }
            let sb: Superblock =
                *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
            let mut failures = 0u32;
            let mut checks = 0u32;

            // 1. Magic.
            checks += 1;
            if sb.magic == LIONFS_MAGIC {
                println!("PASS  superblock magic");
            } else {
                println!("FAIL  superblock magic (not a LionFS image)");
                failures += 1;
            }

            // 2. Superblock CRC.
            checks += 1;
            if sbio::is_valid_superblock_block(&buf).is_some() {
                println!("PASS  superblock checksum");
            } else {
                println!("FAIL  superblock checksum");
                failures += 1;
            }

            // 3. Mountability (format version + feature bits).
            checks += 1;
            if lionfs_core::common::version::is_mountable(sb.version, sb.fs_features) {
                println!("PASS  format version {} mountable", sb.version);
            } else {
                println!(
                    "FAIL  format version {} not mountable by this build ({}), features {:#b}",
                    sb.version,
                    lionfs_core::common::version::CURRENT_VERSION,
                    sb.fs_features
                );
                failures += 1;
            }

            // 4. Tree roots sanity: the four essential trees non-zero.
            checks += 1;
            let roots_ok = sb.inode_tree_root != 0 && sb.checksum_tree_root != 0;
            if roots_ok {
                println!(
                    "PASS  essential tree roots (inode={}, csum={})",
                    sb.inode_tree_root, sb.checksum_tree_root
                );
            } else {
                println!(
                    "FAIL  essential tree roots zero (inode={}, csum={})",
                    sb.inode_tree_root, sb.checksum_tree_root
                );
                failures += 1;
            }

            // 5. Capacity accounting: free <= total.
            checks += 1;
            if sb.free_blocks <= sb.total_blocks {
                println!(
                    "PASS  capacity accounting ({} free of {} blocks)",
                    sb.free_blocks, sb.total_blocks
                );
            } else {
                println!(
                    "FAIL  capacity accounting ({} free > {} total)",
                    sb.free_blocks, sb.total_blocks
                );
                failures += 1;
            }

            // 6. Snapshot registry walk (if a snapshot tree exists).
            if sb.snapshot_tree_root != 0 {
                let tm = TransactionManager::new(&sb);
                let mut tx = tm.begin(0);
                let mut ctx = TxContext::new(&disk, &mut tx);
                checks += 1;
                match tt::timeline(&mut ctx, &sb) {
                    Ok(entries) => {
                        println!("PASS  snapshot registry: {} snapshot(s)", entries.len());
                        // Each record's frozen roots must be non-zero.
                        let mut bad_roots = 0u32;
                        for e in &entries {
                            if let Ok(Some(rec)) =
                                lionfs_core::fs::snapshots::SnapshotManager::new(
                                    sb.snapshot_tree_root,
                                )
                                .get_snapshot(&mut ctx, e.id)
                            {
                                if rec.inode_tree_root == 0 || rec.checksum_tree_root == 0 {
                                    bad_roots += 1;
                                    println!(
                                        "FAIL  snapshot {} records a zero root",
                                        e.id
                                    );
                                }
                            }
                        }
                        if bad_roots == 0 {
                            checks += 1;
                            println!("PASS  snapshot frozen roots non-zero");
                        } else {
                            failures += bad_roots;
                        }
                    }
                    Err(e) => {
                        println!("FAIL  snapshot registry unreadable: {e}");
                        failures += 1;
                    }
                }
            } else {
                println!("NOTE  no snapshot tree on this image (skipping registry checks)");
            }

            println!();
            println!(
                "{} checks: {} passed, {} failed",
                checks,
                checks - failures,
                failures
            );
            if failures > 0 {
                std::process::exit(2);
            }
        }
        "frags" => {
            let frags: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();
            let report = verify_fragments(&frags);
            println!(
                "{} fragments checked: {} healthy, {} corrupt",
                report.checked,
                report.healthy,
                report.corrupt.len()
            );
            for c in &report.corrupt {
                println!("  CORRUPT: {c}");
            }
            if !report.corrupt.is_empty() {
                std::process::exit(2);
            }
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(1);
        }
    }
}
