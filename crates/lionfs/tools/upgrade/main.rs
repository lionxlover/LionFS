//! `lfs_upgrade` -- the Format Vault offline upgrade tool (3.6: REAL).
//!
//! 3.5's binary printed a JSON ok without touching the device. 3.6:
//!
//! * `lfs_upgrade <image> --check`  run the conformance battery, exit
//!   non-zero on any failure (the dry run).
//! * `lfs_upgrade <image> [--min-iterations N]` -- the upgrade:
//!   1. run the conformance battery and REFUSE on any failure;
//!   2. stamp the image with THIS build's known feature registry
//!      (`fs_features` -- bits unknown to this build would already
//!      have failed the battery) and refresh `node_generation` to the
//!      current build's stamp high-water;
//!   3. rewrite all superblock slots atomically (`write_all_slots`).
//!
//! The image MUST be offline (unmounted). An upgrade is idempotent:
//! running it twice changes nothing the second time. Version stays 2
//! by policy -- 3.6 capabilities are feature bits, so a "v2 image"
//! and a "3.6 image" differ only in which bits are set; the tool
//! never invents structures that are not already on disk.

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
        eprintln!("Usage: lfs_upgrade <image> [--check]");
        eprintln!("  --check   conformance battery only (dry run)");
        eprintln!("  default   validate, then commit this build's feature registry");
        eprintln!("            to all superblock slots (offline; idempotent)");
        std::process::exit(1);
    }
    let image = &args[1];
    let check_only = args.iter().any(|a| a == "--check");

    let mut disk = match Disk::open(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: cannot open {image}: {e}");
            std::process::exit(1);
        }
    };
    let sb = read_sb(&disk);

    println!("Step 1/2: conformance battery...");
    let report = conformance::run(&disk, &sb, 64);
    println!("{}", report.render());
    if !report.all_passed() {
        eprintln!("UPGRADE REFUSED: the image does not conform; fix before upgrading.");
        std::process::exit(2);
    }

    if check_only {
        println!("--check: image conforms to this build's format expectations.");
        return;
    }

    println!("Step 2/2: committing feature registry + stamp high-water...");
    let mut new_sb = sb;
    let before = new_sb.fs_features;
    // Keep every bit the image already carries (they are all known --
    // the battery verified that) and add nothing: upgrade never
    // invents structures that are not on disk. The REGISTRY COMMIT is
    // the point: a future build that reads this superblock sees a
    // deliberate, validated feature set.
    new_sb.fs_features &= lionfs_core::common::version::KNOWN_FS_FEATURES;
    new_sb.node_generation = new_sb.node_generation.max(lionfs_core::btree::tree::node_gen_current());
    new_sb.checksum = lionfs_core::utils::checksum::calculate_superblock_checksum(&new_sb);
    lionfs_core::ondisk::superblock::write_all_slots(&mut disk, &new_sb, sb.generation)
        .expect("persist superblock slots");

    println!(
        "Upgrade complete: format version {} (policy: version stays; features are bits), features {:#b} -> {:#b}, node_generation {}.",
        new_sb.version, before, new_sb.fs_features, new_sb.node_generation
    );
    println!("The image is committed to this build's known feature registry.");
}
