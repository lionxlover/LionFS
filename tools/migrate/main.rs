//! `lfs_migrate` -- foreign-filesystem detection & import planning
//! (RFC-004 §9).
//!
//! * `lfs_migrate detect <device-or-image>` -- reads the first
//!   superblock region and reports the detected filesystem.
//! * `lfs_migrate plan <source-tag> <used-gb> <mounted 0|1>` -- prints
//!   the import plan (strategy, sign-off, progress steps).
//! * `lfs_migrate demo` -- runs detection against synthetic superblock
//!   images for every rule in the table (the CI portability artifact).

use std::env;
use std::io::Read;
use std::process::exit;

use lionfs_core::migrate::detect::detect as detect_fs;
use lionfs_core::migrate::plan::ImportPlan;
use lionfs_core::migrate::{FsKind, MAGIC_TABLE};

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("detect") => detect_cmd(args.get(2).map(String::as_str)),
        Some("plan") => plan_cmd(&args),
        Some("demo") | None => demo(),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: lfs_migrate [detect <dev> | plan <tag> <used-gb> <mounted 0|1> | demo]");
    exit(1);
}

fn detect_cmd(path: Option<&str>) {
    let Some(path) = path else { usage() };
    // Read the largest offset any rule needs (btrfs at 0xFF00 + 8).
    let need = 65_280 + 8;
    let mut image = vec![0u8; need];
    match std::fs::File::open(path) {
        Ok(mut f) => {
            if let Err(e) = f.read_exact(&mut image) {
                eprintln!("short read on {path} (need {need} bytes): {e}");
                exit(2);
            }
        }
        Err(e) => {
            eprintln!("cannot open {path}: {e}");
            exit(2);
        }
    }
    match detect_fs(&image) {
        Some(kind) => {
            let p = ImportPlan::new(kind, 0, true);
            println!("{path}: {} ({}) -- import strategy {}", kind.tag(), kind, p.strategy.tag());
        }
        None => {
            println!("{path}: no known filesystem magic; driver-claim or raw-block path");
            exit(3);
        }
    }
}

fn plan_cmd(args: &[String]) {
    let tag = args.get(2).map(String::as_str).unwrap_or("ext4");
    let used_gb: u64 = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let mounted = args.get(4).map(|s| s != "0").unwrap_or(true);
    let kind = FsKind::from_tag(tag).unwrap_or(FsKind::Other);
    let used = used_gb << 30;
    let p = ImportPlan::new(kind, used, mounted);
    println!("import plan for {tag} ({used_gb} GiB used, mounted={mounted}):");
    println!("  strategy:    {}", p.strategy.tag());
    println!("  reason:      {}", p.reason);
    println!("  sign-off:    {}", if p.needs_operator_signoff { "REQUIRED" } else { "not needed" });
    println!("  unattended:  {}", if p.unattended_ok() { "yes (cron/CI safe)" } else { "no" });
    let (lo, hi) = p.estimated_dest_bytes();
    println!("  dest size:   {}-{} GiB (post-pipeline estimate)", lo >> 30, hi >> 30);
    println!("  progress:    {} steps", p.progress_steps);
}

fn demo() {
    println!("LionFS migration detection demo (RFC-004 §9.1) -- {} rules", MAGIC_TABLE.len());
    let mut pass = 0;
    for rule in &MAGIC_TABLE {
        let mut image = vec![0u8; 65_288];
        image[rule.offset..rule.offset + rule.magic.len()].copy_from_slice(rule.magic);
        match detect_fs(&image) {
            Some(kind) if kind == rule.kind => {
                let strategy = ImportPlan::new(kind, 0, true).strategy;
                println!(
                    "  [ok]   {:>8} magic @ {:#06x} -> {} ({} import)",
                    rule.kind.tag(),
                    rule.offset,
                    kind.tag(),
                    strategy.tag()
                );
                pass += 1;
            }
            other => {
                println!("  [FAIL] expected {:?}, got {other:?}", rule.kind);
            }
        }
    }
    // Blank image detects nothing.
    let blank = vec![0u8; 65_288];
    println!("  [ok]   blank image -> no match (None)");
    if detect_fs(&blank).is_none() {
        pass += 1;
    }
    println!("{pass}/{} checks passed", MAGIC_TABLE.len() + 1);
    if pass != MAGIC_TABLE.len() + 1 {
        exit(1);
    }
}
