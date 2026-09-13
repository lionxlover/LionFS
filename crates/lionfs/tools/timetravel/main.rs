//! `lfs_timetravel` — path-based time travel over an image's snapshot
//! timeline (the LionFS 8.0 HFS-merge capability, tool surface).
//!
//! The timeline is the snapshot sequence; a unix timestamp maps to the
//! newest snapshot taken at or before it (same-second ties resolve to
//! the earliest-taken, the conservative point-in-time answer — see
//! `fs::timetravel::snapshot_at_or_before`).
//!
//! Commands:
//!   list   <image>                       — the timeline (oldest first)
//!   stat   <image> <path> <unix_ts>      — resolve a path at a time
//!   cat    <image> <path> <unix_ts>      — read a file's bytes at a time
//!   ls     <image> [path] <unix_ts>      — list a directory at a time
//!   diff   <image> <snap_a> <snap_b>     — path-level diff between snapshots
//!
//! Times also accept `@<id>` to address a snapshot by id directly.

use lionfs_core::disk::block_io::Disk;
use lionfs_core::fs::timetravel as tt;
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
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

/// `@id` addresses a snapshot directly; otherwise a unix timestamp.
/// Returns (kind, value): kind 'i' = snapshot id, 't' = unix seconds.
fn parse_time(spec: &str) -> (char, u64) {
    if let Some(id) = spec.strip_prefix('@') {
        let id: u64 = id.parse().unwrap_or_else(|_| {
            eprintln!("ERROR: bad snapshot id {spec:?}");
            std::process::exit(1);
        });
        ('i', id)
    } else {
        let t: u64 = spec.parse().unwrap_or_else(|_| {
            eprintln!("ERROR: bad timestamp {spec:?} (unix seconds, or @<snapshot_id>)");
            std::process::exit(1);
        });
        ('t', t)
    }
}

/// Resolve a time spec against the timeline: `@id` needs no timeline
/// walk; `t` goes through `snapshot_at_or_before`.
fn time_of(ctx: &mut TxContext, sb: &Superblock, spec: &str) -> u64 {
    let (kind, v) = parse_time(spec);
    if kind == 'i' {
        // Find this snapshot's creation time so the timestamp-based
        // resolver selects exactly it.
        let snap = lionfs_core::fs::snapshots::SnapshotManager::new(sb.snapshot_tree_root);
        match snap.get_snapshot(ctx, v) {
            Ok(Some(rec)) => rec.creation_time,
            _ => {
                eprintln!("ERROR: snapshot {v} not found");
                std::process::exit(1);
            }
        }
    } else {
        v
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("LionFS time travel {} ({})", lionfs_core::VERSION, lionfs_core::EDITION);
        eprintln!("Usage: lfs_timetravel <list|stat|cat|ls|diff> <image> [args]");
        eprintln!("  list <image>                    — snapshot timeline, oldest first");
        eprintln!("  stat <image> <path> <time>      — resolve a path at a time");
        eprintln!("  cat  <image> <path> <time>      — read a file's bytes at a time");
        eprintln!("  ls   <image> [path] <time>      — list a directory at a time");
        eprintln!("  diff <image> <snap_a> <snap_b>  — path-level diff (+/-/~)");
        eprintln!();
        eprintln!("  <time> is unix seconds, or @<snapshot_id> to address a snapshot directly.");
        std::process::exit(1);
    }
    let cmd = args[1].as_str();
    let image = &args[2];

    let disk = match Disk::open(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: cannot open {image}: {e}");
            std::process::exit(1);
        }
    };
    let sb = read_sb(&disk);
    if sb.snapshot_tree_root == 0 {
        eprintln!("ERROR: image has no snapshot tree");
        std::process::exit(1);
    }
    let tm = TransactionManager::new(&sb);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);

    match cmd {
        "list" => match tt::timeline(&mut ctx, &sb) {
            Ok(entries) => {
                if entries.is_empty() {
                    println!("no snapshots — the timeline is empty");
                    return;
                }
                println!("ID\tCREATED(unix)\tGEN");
                for e in entries {
                    println!("{}\t{}\t{}", e.id, e.created_at_unix, e.generation);
                }
            }
            Err(e) => {
                eprintln!("ERROR: {e}");
                std::process::exit(1);
            }
        },
        "stat" => {
            if args.len() < 5 {
                eprintln!("ERROR: stat needs <path> <time>");
                std::process::exit(1);
            }
            let path = &args[3];
            let t = time_of(&mut ctx, &sb, &args[4]);
            match tt::resolve_path_at(&mut ctx, &sb, path, t) {
                Ok(r) => {
                    println!("path:      {}", r.path);
                    println!("inode:     {}", r.ino);
                    println!(
                        "type:      {}",
                        if r.file_type == 4 {
                            "directory"
                        } else if r.file_type == 10 {
                            "symlink"
                        } else {
                            "regular"
                        }
                    );
                    println!("size:      {}", r.size);
                    println!("mtime:     {}", r.mtime_unix);
                    println!(
                        "snapshot:  {} (taken at {})",
                        r.snapshot_id, r.requested_at_unix
                    );
                }
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            }
        }
        "cat" => {
            if args.len() < 5 {
                eprintln!("ERROR: cat needs <path> <time>");
                std::process::exit(1);
            }
            let path = &args[3];
            let t = time_of(&mut ctx, &sb, &args[4]);
            match tt::read_file_at(&mut ctx, &sb, path, t) {
                Ok(data) => {
                    use std::io::Write;
                    std::io::stdout().write_all(&data).unwrap();
                }
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            }
        }
        "ls" => {
            if args.len() < 4 {
                eprintln!("ERROR: ls needs <time> (and optionally <path>)");
                std::process::exit(1);
            }
            // Accept both `ls <img> <time>` and `ls <img> <path> <time>`.
            let (path, tspec) = if args.len() >= 5 {
                (args[3].clone(), args[4].clone())
            } else {
                ("/".to_string(), args[3].clone())
            };
            let t = time_of(&mut ctx, &sb, &tspec);
            match tt::list_dir_at(&mut ctx, &sb, &path, t) {
                Ok(entries) => {
                    for (name, ft, ino) in entries {
                        let kind = match ft {
                            4 => "d",
                            10 => "l",
                            _ => "-",
                        };
                        println!("{kind} {ino:>10}  {name}");
                    }
                }
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            }
        }
        "diff" => {
            if args.len() < 5 {
                eprintln!("ERROR: diff needs <snap_a> <snap_b>");
                std::process::exit(1);
            }
            let a: u64 = args[3]
                .trim_start_matches('@')
                .parse()
                .unwrap_or_else(|_| {
                    eprintln!("ERROR: bad snapshot id {:?}", args[3]);
                    std::process::exit(1);
                });
            let b: u64 = args[4]
                .trim_start_matches('@')
                .parse()
                .unwrap_or_else(|_| {
                    eprintln!("ERROR: bad snapshot id {:?}", args[4]);
                    std::process::exit(1);
                });
            match tt::diff(&mut ctx, &sb, a, b) {
                Ok(diffs) => {
                    if diffs.is_empty() {
                        println!("no differences between snapshot {a} and snapshot {b}");
                    }
                    for d in &diffs {
                        println!(
                            "{} {:>10} {:>10}  {}",
                            d.kind_symbol(),
                            d.size_a.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
                            d.size_b.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
                            d.path
                        );
                    }
                }
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(1);
        }
    }
}
