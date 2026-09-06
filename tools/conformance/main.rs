//! `lfs_conformance` -- the Format Vault image checker (offline).
//!
//! Runs the full conformance battery (`ondisk::conformance`) against a
//! LionFS image: superblock integrity + slot agreement, geometry,
//! tree reachability and structural validity, checksum spot-
//! verification against on-disk bytes, snapshot/clone/xattr registry
//! sanity, bitmap/free-block agreement, journal tail. Read-only:
//! the battery never writes to the image.
//!
//! Exit codes: 0 = all checks pass, 2 = failures, 1 = usage/IO error.
//! `--json` emits one JSON object instead of the human report.

use lionfs_core::disk::block_io::Disk;
use lionfs_core::ondisk::conformance;
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};

fn read_sb(disk: &Disk) -> Superblock {
    let mut buf = [0u8; BLOCK_SIZE];
    disk.read_block(0, &mut buf).expect("read superblock");
    let sb: Superblock = *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
    if sb.magic != LIONFS_MAGIC {
        eprintln!("ERROR: not a LionFS image (bad magic)");
        std::process::exit(1);
    }
    sb
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: lfs_conformance <image> [--json] [--verify-blocks N]");
        eprintln!("Runs the Format Vault conformance battery (read-only).");
        std::process::exit(1);
    }
    let image = &args[1];
    let json = args.iter().any(|a| a == "--json");
    let verify_blocks = args
        .iter()
        .position(|a| a == "--verify-blocks")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);

    let disk = match Disk::open(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: cannot open {image}: {e}");
            std::process::exit(1);
        }
    };
    let sb = read_sb(&disk);
    let report = conformance::run(&disk, &sb, verify_blocks);

    if json {
        println!("{{");
        println!("  \"image\": \"{}\",", image.replace('\\', "\\\\"));
        println!("  \"all_passed\": {},", report.all_passed());
        println!("  \"checks\": [");
        let n = report.checks.len();
        for (i, c) in report.checks.iter().enumerate() {
            let detail = c.detail.replace('\\', "\\\\").replace('"', "\\\"");
            println!(
                "    {{\"name\": \"{}\", \"passed\": {}, \"detail\": \"{}\"}}{}",
                c.name,
                c.passed,
                detail,
                if i + 1 < n { "," } else { "" }
            );
        }
        println!("  ]");
        println!("}}");
    } else {
        println!("{}", report.render());
    }
    std::process::exit(if report.all_passed() { 0 } else { 2 });
}
