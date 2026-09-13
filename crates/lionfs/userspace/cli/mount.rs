use std::env;

use lionfs_core::disk::block_io::Disk;
use lionfs_core::fs::filesystem::LionFS;
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE};
use lionfs_core::pool::raid::RaidProfile;

fn main() {
    env_logger::init();
    // Readahead defaults off (measured negative on buffered reads);
    // LFS_READAHEAD=1 opts in.
    lionfs_core::file::writer::init_readahead_from_env();

    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: mount_lfs <image_file> <mountpoint> [device2] [device3] ...");
        eprintln!("Extra device paths (for a multi-device RAID pool) are optional and must be");
        eprintln!("given in the same order they were passed to mkfs_lfs.");
        std::process::exit(1);
    }

    let image_file = &args[1];
    let mountpoint = &args[2];
    // Phase 4: optional -o <options> (comma-separated, mount(8) style),
    // e.g. `mount_lfs img mnt -o zstd_level=6,ro`. Remaining args are
    // pool devices.
    let mut option_str = String::new();
    let mut rest: Vec<String> = Vec::new();
    let mut it = args[3..].iter();
    while let Some(a) = it.next() {
        if a == "-o" {
            option_str = it.next().cloned().unwrap_or_default();
        } else {
            rest.push(a.clone());
        }
    }
    let extra_devices: Vec<String> = rest;
    let mount_config = lionfs_core::common::config::MountConfig::from_options_str(&option_str);
    // Apply the zstd level to this process's compression path.
    lionfs_core::fs::compression::set_zstd_level(mount_config.zstd_level);
    if mount_config.zstd_level != 3 {
        println!("zstd level: {}", mount_config.zstd_level);
    }

    // Bootstrap: block 0 (the superblock) is written identically to every
    // device regardless of RAID profile specifically so it can be read
    // this way -- via a plain single-device open of just the first device
    // -- before we know the pool's actual RAID profile, which is a field
    // inside the superblock we're about to read.
    let bootstrap = Disk::open(image_file).expect("Failed to open image file");
    let mut sb_buf = [0u8; BLOCK_SIZE];
    bootstrap
        .read_block(0, &mut sb_buf)
        .expect("Failed to read superblock");
    let sb: Superblock = *bytemuck::from_bytes(&sb_buf);
    drop(bootstrap);

    // Refuse filesystems this build cannot understand: a NEWER format
    // version (whose fields this build could silently misinterpret)
    // OR unknown fs_features bits (the 3.6 Format Vault gate; the
    // core's LionFS::new enforces the same rule for library callers).
    if !lionfs_core::common::version::is_mountable(sb.version, sb.fs_features) {
        eprintln!(
            "Refusing to mount: format version {} with feature bits {:#x} exceeds this build (version {}, known bits {:#x})",
            sb.version,
            sb.fs_features,
            lionfs_core::common::version::CURRENT_VERSION,
            lionfs_core::common::version::KNOWN_FS_FEATURES
        );
        std::process::exit(1);
    }

    // 3.6: volume key envelope gate. An image with a passphrase
    // envelope cannot unlock without it (3 attempts, the key_flow
    // lockout budget).
    if sb.key_envelope_block != 0 {
        let pool_disk = Disk::open(image_file).expect("reopen for envelope");
        let mut env_buf = [0u8; BLOCK_SIZE];
        pool_disk
            .read_block(sb.key_envelope_block, &mut env_buf)
            .expect("read key envelope block");
        drop(pool_disk);
        match lionfs_core::security::kdf::envelope_from_block(&env_buf) {
            Ok(blob) => {
                let passphrase = std::env::var("LFS_PASSPHRASE").unwrap_or_default();
                if passphrase.is_empty() {
                    eprintln!(
                        "This volume is passphrase-protected. Set LFS_PASSPHRASE (or mount from a terminal that provides it)."
                    );
                    std::process::exit(1);
                }
                if lionfs_core::security::kdf::EnvelopeV2::unwrap(passphrase.as_bytes(), &blob)
                    .is_err()
                {
                    eprintln!("Passphrase rejected (envelope authentication failed).");
                    std::process::exit(1);
                }
                println!("Volume unlocked (envelope v2: kdf={}, aead={}).", blob.kdf_id, blob.aead_id);
            }
            Err(e) => {
                eprintln!("Refusing to mount: key envelope unreadable ({e}).");
                std::process::exit(1);
            }
        }
    }

    let profile = RaidProfile::from_u8(sb.raid_profile);
    let device_count;
    let disk = if profile == RaidProfile::Single {
        if !extra_devices.is_empty() {
            eprintln!("Warning: extra device paths given but this filesystem's superblock says RAID profile is Single; ignoring them.");
        }
        device_count = 1;
        Disk::open(image_file).expect("Failed to open image file")
    } else {
        let mut all_devices = vec![image_file.clone()];
        all_devices.extend(extra_devices);
        if all_devices.len() < profile.min_devices() {
            eprintln!(
                "This filesystem's superblock says RAID profile {:?}, which needs at least {} devices, but only {} were given.",
                profile, profile.min_devices(), all_devices.len()
            );
            std::process::exit(1);
        }
        device_count = all_devices.len();
        Disk::open_pool(&all_devices, profile, sb.chunk_size).expect("Failed to open device pool")
    };

    let fs = LionFS::new(disk, image_file.to_string()).expect("Failed to mount LionFS");

    let options = lionfs_core::mount::options::build_mount_options(
        &lionfs_core::common::config::MountConfig {
            read_only: false,
            ..Default::default()
        },
    );

    println!(
        "Mounting {} ({:?}, {} device(s)) to {}",
        image_file, profile, device_count, mountpoint
    );
    // 2.0: the engine never implements fuser's trait directly; it goes
    // through the platform-neutral VfsOps and the FUSE bridge.
    let bridge = lionfs_core::mount::mount::fuse_bridge(fs);
    fuser::mount2(bridge, mountpoint, &options).unwrap();
}
