//! `lfs_dedupe` — CDC dedup analysis for LionFS (LionFS 8.0: the
//! HFS-merge capability behind a real tool — this was a banner stub
//! through 7.1).
//!
//! Modes:
//!   lfs_dedupe image <image> <path-in-image>
//!       FastCDC-chunk a file stored on the image (read through the
//!       live trees) and report the dedup analysis.
//!   lfs_dedupe host <file> [more files...]
//!       Analyze host files: one file alone (self-dedup), or several
//!       as one logical stream (cross-file dedup potential — the
//!       order matters, later files probe the index built from
//!       earlier ones).
//!   lfs_dedupe demo
//!       Self-contained demonstration: the shifted-input CDC property
//!       and the convergent-encryption stability/isolation proof.
//!
//! All analysis uses domain-scoped convergent identities, so the
//! numbers predict cluster-plane behavior (dedup that keeps working
//! with encryption ON).

use lionfs_core::disk::block_io::Disk;
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
use lionfs_core::pipeline::cdc_dedup::{
    analyze, analyze_pair, convergent_roundtrip, read_live_file, DEFAULT_CDC,
};

use lionfs_cluster::crypto::{Cipher, KeyTree};
use lionfs_cluster::dedup::DomainId;
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;

fn print_stats(stats: &lionfs_core::pipeline::cdc_dedup::CdcDedupStats) {
    println!("  logical bytes:     {}", stats.logical_bytes);
    println!("  chunks:            {} (avg {:.0} B, min {}, max {})",
        stats.chunks, stats.avg_chunk_bytes(), stats.min_chunk_bytes, stats.max_chunk_bytes);
    println!("  unique chunks:     {}", stats.unique_chunks);
    println!("  stored bytes:      {}", stats.stored_bytes);
    println!("  eliminated bytes:  {}", stats.dedup_bytes);
    println!("  dedup ratio:       {:.3}x", stats.ratio());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("LionFS CDC dedup analysis {} ({})", lionfs_core::VERSION, lionfs_core::EDITION);
        eprintln!("Usage:");
        eprintln!("  lfs_dedupe image <image> <path-in-image>   — analyze a file on an image");
        eprintln!("  lfs_dedupe host  <file> [file2 ...]        — analyze host files (cross-file order)");
        eprintln!("  lfs_dedupe demo                             — CDC + convergent-encryption demo");
        std::process::exit(1);
    }

    let domain = DomainId::root();
    let cfg = DEFAULT_CDC;

    match args[1].as_str() {
        "image" => {
            if args.len() < 4 {
                eprintln!("ERROR: image mode needs <image> <path-in-image>");
                std::process::exit(1);
            }
            let image = &args[2];
            let path = &args[3];
            let disk = match Disk::open(image) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("ERROR: cannot open {image}: {e}");
                    std::process::exit(1);
                }
            };
            let mut buf = [0u8; BLOCK_SIZE];
            if disk.read_block(0, &mut buf).is_err() {
                eprintln!("ERROR: cannot read superblock");
                std::process::exit(1);
            }
            let sb: Superblock =
                *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
            if sb.magic != LIONFS_MAGIC {
                eprintln!("ERROR: not a LionFS image (bad magic)");
                std::process::exit(1);
            }
            let tm = TransactionManager::new(&sb);
            let mut tx = tm.begin(0);
            let mut ctx = TxContext::new(&disk, &mut tx);
            let data = match read_live_file(&mut ctx, &sb, path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            };
            println!("LionFS CDC dedup analysis — {image}:{path}");
            let (stats, _) = analyze(&data, &domain, &cfg);
            print_stats(&stats);
        }
        "host" => {
            if args.len() < 3 {
                eprintln!("ERROR: host mode needs at least one file");
                std::process::exit(1);
            }
            let files: Vec<Vec<u8>> = args[2..]
                .iter()
                .map(|p| {
                    std::fs::read(p).unwrap_or_else(|e| {
                        eprintln!("ERROR: cannot read {p}: {e}");
                        std::process::exit(1);
                    })
                })
                .collect();
            let (stats, _) = if files.len() == 1 {
                println!("LionFS CDC dedup analysis — {}", args[2]);
                analyze(&files[0], &domain, &cfg)
            } else {
                println!(
                    "LionFS CDC dedup analysis — {} files as one logical stream",
                    files.len()
                );
                let first = files[0].clone();
                let rest: Vec<u8> = files[1..].concat();
                // Two-slice pair analysis preserves cross-file probing;
                // for >2 files, fold by re-analyzing the concatenation.
                if files.len() == 2 {
                    analyze_pair(&first, &rest, &domain, &cfg)
                } else {
                    analyze(&[first, rest].concat(), &domain, &cfg)
                }
            };
            print_stats(&stats);
        }
        "demo" => {
            println!("LionFS 8.0 dedup demo (FastCDC + convergent encryption)");
            println!();
            // 1. The shifted-input CDC property (pseudo-random payload:
            //    gear-hash boundaries fire naturally, unlike toy
            //    arithmetic patterns that always cut at hard-max).
            let mut s: u64 = 0x9E3779B97F4A7C15;
            let mut next = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            };
            let base: Vec<u8> = (0..256 * 1024).map(|_| next()).collect();
            let mut shifted = base[..1000].to_vec();
            shifted.extend_from_slice(&base);
            let (stats, _) = analyze_pair(&base, &shifted, &domain, &cfg);
            println!("[CDC] 256 KiB original + same data with a 1 KiB prefix inserted:");
            println!(
                "      {:.1}% of the shifted copy still dedups (fixed-size blocks would lose ~100%)",
                100.0 * stats.dedup_bytes as f64 / (1000 + 256 * 1024) as f64
            );
            println!("      ratio {:.3}x over the logical stream", stats.ratio());
            println!();
            // 2. Convergent encryption.
            let keytree = KeyTree::generate();
            let chunk = &base[..8192];
            let (stable, isolated) =
                convergent_roundtrip(chunk, &domain, &keytree, Cipher::Aes256Gcm);
            println!("[Convergent crypto] same (domain, content) encrypts identically: {stable}");
            println!("[Convergent crypto] different domains encrypt differently: {isolated}");
            println!("      dedup keeps working WITH encryption on — the property");
            println!("      per-file random keys can never offer.");
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(1);
        }
    }
}
