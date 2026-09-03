//! lfs_ioperf -- in-process I/O core benchmark for LionFS.
//!
//! WHY THIS EXISTS INSTEAD OF fio-ON-MOUNT NUMBERS
//! -----------------------------------------------
//! The development container for this repo has no /dev/fuse, so
//! mounting LionFS and running fio against it is impossible here. This
//! tool instead drives the *real* I/O core directly -- FileManager
//! reads/writes, the bitmap allocator, the checksum tree, and the
//! transaction layer -- against an image file, in-process.
//!
//! These numbers measure USER-SPACE CPU cost of the LionFS I/O path on
//! this machine. They are NOT comparable to fio-against-a-mount
//! numbers: there is no FUSE round trip, no kernel page cache and no
//! context switches in either direction. They are suitable for
//! before/after comparisons *within this harness only* -- which is
//! exactly what the phase plan asks for (per-change attribution), and
//! they will be labeled as such everywhere results are quoted.
//!
//! USAGE
//! -----
//!   lfs_ioperf [--blocks N] [--secs T] [--checksums] [--json]
//!
//! Patterns (all 4 KiB block granularity, matching the plan's profile):
//!   seq-write   : sequential 4 KiB-aligned writes, fresh file
//!   seq-read    : sequential 4 KiB reads of the file just written
//!   rand4k-write: 4 KiB writes at PRNG-shuffled offsets
//!   rand4k-read : 4 KiB reads at PRNG-shuffled offsets
//!
//! The default image is a temp file under $TMPDIR. Results go to
//! stdout (human) or benches/results/ (JSON) with --json.

use lionfs_core::allocator::bitmap::Allocator;
use lionfs_core::disk::block_io::Disk;
use lionfs_core::file::writer::FileManager;
use lionfs_core::ondisk::serialization::{
    BlockGroupDescriptor, Extent, Inode, Superblock, BLOCK_SIZE,
};
use lionfs_core::security::block_cipher::BlockCipherContext;
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;
use std::fs;
use std::time::{Duration, Instant};

struct Args {
    blocks: u64,
    secs: f64,
    checksums: bool,
    json: bool,
    image: Option<String>,
    /// RAID profile for the pool ("single", "raid5", "raid6").
    profile: String,
    /// Number of devices in the pool.
    devices: usize,
    /// Chunk size in blocks (0 = recommended default).
    chunk: u32,
    /// Phase 4: run the compression-cluster benchmark (corpus ratio +
    /// throughput + level sweep).
    compress: bool,
    /// zstd level for the compression benchmark (default 3).
    zstd_level: i32,
}

fn parse_args() -> Args {
    let mut a = Args {
        blocks: 65536,
        secs: 5.0,
        checksums: true,
        json: false,
        image: None,
        profile: "single".into(),
        devices: 1,
        chunk: 0,
        compress: false,
        zstd_level: 3,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--blocks" => {
                a.blocks = argv[i + 1].parse().unwrap();
                i += 2;
            }
            "--secs" => {
                a.secs = argv[i + 1].parse().unwrap();
                i += 2;
            }
            "--image" => {
                a.image = Some(argv[i + 1].clone());
                i += 2;
            }
            "--no-checksums" => {
                a.checksums = false;
                i += 1;
            }
            "--json" => {
                a.json = true;
                i += 1;
            }
            "--profile" => {
                a.profile = argv[i + 1].clone();
                i += 2;
            }
            "--devices" => {
                a.devices = argv[i + 1].parse().unwrap();
                i += 2;
            }
            "--chunk" => {
                a.chunk = argv[i + 1].parse().unwrap();
                i += 2;
            }
            "--compress" => {
                a.compress = true;
                i += 1;
            }
            "--zstd-level" => {
                a.zstd_level = argv[i + 1].parse().unwrap();
                a.zstd_level = a.zstd_level.clamp(1, 22);
                i += 2;
            }
            _ => {
                eprintln!("unknown arg {}", argv[i]);
                std::process::exit(2);
            }
        }
    }
    if a.devices < 1 {
        eprintln!("--devices must be >= 1");
        std::process::exit(2);
    }
    a
}

struct ResultLine {
    pattern: &'static str,
    mib_per_s: f64,
    ops_per_s: f64,
    bytes: u64,
    secs: f64,
    extents: u64,       // inline extent count of the inode after the run
    spill_entries: u64, // entries in the per-inode spill tree (0 if none)
    spilled: bool,
    checksums: bool,
}

/// Count a file's total extent fragments (inline + spilled) -- the
/// fragmentation metric that matters for read-path mapping cost.
fn count_extents(ctx: &mut TxContext, inode: &Inode) -> (u64, u64) {
    let inline = inode.extent_count as u64;
    let spill = if inode.spill_extent_root != 0 {
        lionfs_core::extents::tree::ExtentTree::new(inode.spill_extent_root)
            .iter_extents(ctx)
            .map(|v| v.len() as u64)
            .unwrap_or(0)
    } else {
        0
    };
    (inline, spill)
}

/// Deterministic xorshift64 PRNG (seeded, reproducible).
struct Prng(u64);
impl Prng {
    fn new(seed: u64) -> Self {
        Prng(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const REGION_BLOCKS: u64 = 8192; // 32 MiB working region
const K4: u64 = 1; // 4 KiB unit  (1 block)
const K64: u64 = 16; // 64 KiB unit (16 blocks)

struct Env {
    disk: Disk,
    tm: TransactionManager,
    sb: Superblock,
    blocks: u64,
    bg: BlockGroupDescriptor,
    checksum_tree_root: u64,
    path: String,
    paths: Vec<String>,
}

fn build_env(args: &Args) -> Env {
    use lionfs_core::pool::raid::RaidProfile;
    let profile = match args.profile.as_str() {
        "single" => RaidProfile::Single,
        "raid5" => RaidProfile::Raid5,
        "raid6" => RaidProfile::Raid6,
        "raid0" => RaidProfile::Raid0,
        "raid1" => RaidProfile::Raid1,
        "raid10" => RaidProfile::Raid10,
        _ => {
            eprintln!("unknown profile {}", args.profile);
            std::process::exit(2);
        }
    };
    let n = args.devices.max(profile.min_devices());
    let mut paths: Vec<String> = Vec::new();
    if args.image.is_some() && n == 1 {
        paths.push(args.image.clone().unwrap());
    } else {
        for i in 0..n {
            paths.push(format!(
                "/tmp/lfs_ioperf_{}_dev{}.img",
                std::process::id(),
                i
            ));
        }
    }
    for p in &paths {
        let _ = fs::remove_file(p);
    }
    // RAID address mapping shrinks usable space (parity/mirroring); size
    // devices so the 32 MiB region + metadata always fits.
    let per_device_blocks = args
        .blocks
        .max((REGION_BLOCKS + 4096) as u64 * 2 / n.max(1) as u64 + 4096);
    let chunk = if args.chunk > 0 {
        args.chunk
    } else {
        lionfs_core::pool::raid::RaidEngine::recommended_chunk_size_blocks(0)
    };
    let disk = if n == 1 && args.profile == "single" {
        Disk::create(&paths[0], per_device_blocks * BLOCK_SIZE as u64).expect("create image")
    } else {
        println!(
            "pool: {} x {} devices, chunk {} blocks ({} KiB), per-device {} blocks",
            args.profile,
            n,
            chunk,
            chunk * (BLOCK_SIZE as u32) / 1024,
            per_device_blocks
        );
        Disk::create_pool(
            &paths,
            per_device_blocks * BLOCK_SIZE as u64,
            profile,
            chunk,
        )
        .expect("create pool")
    };
    let path = paths[0].clone();
    let sb = Superblock {
        magic: 0,
        version: 0,
        block_size: BLOCK_SIZE as u32,
        total_blocks: args.blocks,
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
        blocks_per_group: args.blocks as u32,
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
    // Layout note: the bitmap for a 65536-block group spans TWO blocks
    // (1 and 2 -- any block >= 32768 is tracked in bitmap block 2), so
    // the checksum-tree root must NOT live at block 2. It goes at
    // block 8, inside the reserved 0..16 region, clear of both bitmap
    // blocks.
    let checksum_tree_root = if args.checksums { 8 } else { 0 };
    Env {
        disk,
        tm,
        sb,
        blocks: args.blocks,
        bg,
        checksum_tree_root,
        path,
        paths,
    }
}

fn fresh_inode(ino: u64) -> Inode {
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
        compression_algo: 0,
        encryption_algo: 0,
        key_id: 0,
        extents: [Extent {
            logical_start: 0,
            physical_start: 0,
            length: 0,
        }; 7],
        checksum: 0,
        spill_pad_head: [0; 4],
        spill_extent_root: 0,
    }
}

/// Run the sequential group: one fresh write pass over the region
/// (timed), then steady-state write passes until budget, then read
/// passes until budget -- all on the SAME transaction and inode, since
/// reads must see the just-written state. `unit_blocks` is the number
/// of blocks per FileManager call (1 = 4 KiB fio-style, 16 = 64 KiB
/// fio-style, matching the plan's benchmark profile).
fn run_seq_group(
    env: &mut Env,
    secs: f64,
    with_checksums: bool,
    unit_blocks: u64,
    tag: &str,
) -> Vec<ResultLine> {
    let mut out = Vec::new();
    let mut tx = env.tm.begin(0);
    let mut ctx = TxContext::new(&env.disk, &mut tx);
    Allocator::mark_blocks_used(&mut ctx, env.bg.bg_block_bitmap, 0, 16).unwrap();
    if with_checksums && env.checksum_tree_root != 0 {
        lionfs_core::integrity::checksum_tree::ChecksumTree::init_empty(
            &mut ctx,
            env.checksum_tree_root,
        )
        .unwrap();
    }
    let cctx = BlockCipherContext::none();
    let mut inode = fresh_inode(2);
    let bpg = env.blocks as u32;

    // Build a `unit_blocks`-sized write payload whose content depends
    // on the REGION block index (still verifiable round-trip).
    let units = REGION_BLOCKS / unit_blocks;
    let payload = |u: u64| {
        let mut v = Vec::with_capacity(unit_blocks as usize * BLOCK_SIZE);
        for b in 0..unit_blocks {
            let i = u * unit_blocks + b;
            v.extend_from_slice(&vec![(i % 251) as u8; BLOCK_SIZE]);
        }
        v
    };
    let unit_bytes = unit_blocks * BLOCK_SIZE as u64;

    // 1. Fresh sequential write: exactly one pass over the region.
    let t = Instant::now();
    for u in 0..units {
        FileManager::write_file(
            &mut ctx,
            &env.bg,
            bpg,
            env.checksum_tree_root,
            &cctx,
            &mut inode,
            u * unit_bytes,
            &payload(u),
        )
        .unwrap();
    }
    let el = t.elapsed().as_secs_f64();
    out.push(ResultLine {
        pattern: Box::leak(format!("{}-fresh", tag).into_boxed_str()),
        mib_per_s: (REGION_BLOCKS * BLOCK_SIZE as u64) as f64 / (1024.0 * 1024.0) / el,
        ops_per_s: units as f64 / el,
        bytes: REGION_BLOCKS * BLOCK_SIZE as u64,
        secs: el,
        extents: inode.extent_count as u64,
        spill_entries: 0,
        spilled: inode.spill_extent_root != 0,
        checksums: with_checksums && env.checksum_tree_root != 0,
    });

    // Record fragmentation after the fresh pass (the metric the plan
    // cares about: how many extent fragments does a sequentially
    // written file end up with).
    let (frag_inline, frag_spill) = count_extents(&mut ctx, &inode);
    out.last_mut().unwrap().extents = frag_inline;
    out.last_mut().unwrap().spill_entries = frag_spill;

    // 2. Steady-state sequential write: repeated full passes (RMW
    //    overwrite path) until the time budget is spent.
    let t = Instant::now();
    let budget = Duration::from_secs_f64(secs);
    let mut passes: u64 = 0;
    let mut u = 0u64;
    while t.elapsed() < budget {
        FileManager::write_file(
            &mut ctx,
            &env.bg,
            bpg,
            env.checksum_tree_root,
            &cctx,
            &mut inode,
            u * unit_bytes,
            &payload(u),
        )
        .unwrap();
        u += 1;
        if u >= units {
            u = 0;
            passes += 1;
        }
    }
    let el = t.elapsed().as_secs_f64();
    let bytes = (passes * units + u) * unit_bytes;
    out.push(ResultLine {
        pattern: Box::leak(tag.to_string().into_boxed_str()),
        mib_per_s: bytes as f64 / (1024.0 * 1024.0) / el,
        ops_per_s: (passes * units + u) as f64 / el,
        bytes,
        secs: el,
        extents: inode.extent_count as u64,
        spill_entries: 0,
        spilled: inode.spill_extent_root != 0,
        checksums: with_checksums && env.checksum_tree_root != 0,
    });

    // 3. Sequential read of the file just written: full passes until
    //    budget, `unit_bytes` per read call.
    let t = Instant::now();
    let mut passes: u64 = 0;
    let mut u = 0u64;
    while t.elapsed() < budget {
        let got = FileManager::read_file(
            &mut ctx,
            env.checksum_tree_root,
            0,
            &cctx,
            &mut inode,
            u * unit_bytes,
            unit_bytes,
        )
        .unwrap();
        debug_assert_eq!(got.len(), unit_bytes as usize);
        u += 1;
        if u >= units {
            u = 0;
            passes += 1;
        }
    }
    let el = t.elapsed().as_secs_f64();
    let bytes = (passes * units + u) * unit_bytes;
    out.push(ResultLine {
        pattern: Box::leak(
            format!("{}-read", tag.strip_suffix("-write").unwrap_or(tag)).into_boxed_str(),
        ),
        mib_per_s: bytes as f64 / (1024.0 * 1024.0) / el,
        ops_per_s: (passes * units + u) as f64 / el,
        bytes,
        secs: el,
        extents: inode.extent_count as u64,
        spill_entries: 0,
        spilled: inode.spill_extent_root != 0,
        checksums: with_checksums && env.checksum_tree_root != 0,
    });

    out
}

/// Phase 4 compression benchmark. Writes a mixed-compressibility
/// corpus (40% repeating records / 35% dictionary text / 25% PRNG
/// bytes -- NOT artificially-repetitive data, per the plan) through a
/// compressed inode, and reports:
///   - write/read throughput through the cluster path
///   - ON-DISK SPACE actually saved: physical blocks consumed (from the
///     allocator bitmap) vs logical blocks written
///   - a level sweep (1/3/6/9) measuring the ratio/CPU tradeoff with
///     real numbers
fn run_compress_group(env: &mut Env, args: &Args) {
    use lionfs_core::transaction::transaction::TxContext;
    let bpg = env.blocks as u32;
    let cctx = BlockCipherContext::none();
    lionfs_core::fs::compression::set_zstd_level(args.zstd_level);

    let corpus_len = (lionfs_core::file::cluster::CLUSTER_BYTES * 64) as usize; // 8 MiB
    let corpus = mixed_corpus(corpus_len);
    let logical_blocks = (corpus_len as u64).div_ceil(BLOCK_SIZE as u64);

    println!("compress: mixed corpus (40% repeating / 35% dictionary text / 25% PRNG), {} MiB logical ({} blocks)",
        corpus_len / (1024 * 1024), logical_blocks);
    println!(
        "compress: zstd level {} (mount_lfs -o zstd_level=N)",
        args.zstd_level
    );

    {
        let mut tx = env.tm.begin(0);
        let mut ctx = TxContext::new(&env.disk, &mut tx);
        Allocator::mark_blocks_used(&mut ctx, env.bg.bg_block_bitmap, 0, 16).unwrap();
        let mut inode = fresh_inode(2);
        inode.compression_algo = 2; // zstd

        let before = env.blocks
            - Allocator::count_free_blocks(&mut ctx, env.bg.bg_block_bitmap, env.blocks).unwrap();
        let t = Instant::now();
        // 64 KiB per write call, like the plan's sequential profile.
        let unit = 65536usize;
        for off in (0..corpus_len).step_by(unit) {
            let end = (off + unit).min(corpus_len);
            FileManager::write_file(
                &mut ctx,
                &env.bg,
                bpg,
                0,
                &cctx,
                &mut inode,
                off as u64,
                &corpus[off..end],
            )
            .unwrap();
        }
        let write_secs = t.elapsed().as_secs_f64();
        let after = env.blocks
            - Allocator::count_free_blocks(&mut ctx, env.bg.bg_block_bitmap, env.blocks).unwrap();
        let consumed = after - before;

        println!(
            "  write : {:>7.1} MiB/s   ({:.2} s)",
            corpus_len as f64 / (1024.0 * 1024.0) / write_secs,
            write_secs
        );
        println!(
            "  space : {:>7} physical blocks for {} logical -> ratio {:.2}x ({:.1}% of original)",
            consumed,
            logical_blocks,
            logical_blocks as f64 / consumed as f64,
            100.0 * consumed as f64 / logical_blocks as f64
        );

        // Read back: sequential full-file read.
        let t = Instant::now();
        let want_len = inode.size;
        let back = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, 0, want_len).unwrap();
        let read_secs = t.elapsed().as_secs_f64();
        assert_eq!(
            back, corpus,
            "compressed corpus must round-trip byte-identical"
        );
        println!(
            "  read  : {:>7.1} MiB/s   ({:.2} s, byte-identical round-trip verified)",
            corpus_len as f64 / (1024.0 * 1024.0) / read_secs,
            read_secs
        );

        // Random 4 KiB reads through the cluster cache.
        let mut rng = Prng::new(0x5eed_4321);
        let t = Instant::now();
        let iters = 20000u64;
        for _ in 0..iters {
            let off = (rng.next() as usize) % (corpus_len - 4096);
            let got = FileManager::read_file(&mut ctx, 0, 0, &cctx, &mut inode, off as u64, 4096)
                .unwrap();
            assert_eq!(&got[..8], &corpus[off..off + 8]);
        }
        println!(
            "  r4k-rd: {:>7.0} ops/s   (4 KiB reads through the 2 MiB decompressed-cluster LRU)",
            iters as f64 / t.elapsed().as_secs_f64()
        );
        drop(ctx);
        drop(tx);
    }

    // Level sweep: fresh image state each level (new tx + fresh inode
    // reuses the same scratch image; allocations continue from the
    // frontier, so measure per-level consumed blocks from the delta).
    println!("  level sweep (ratio, write MiB/s):");
    for level in [1, 3, 6, 9] {
        lionfs_core::fs::compression::set_zstd_level(level);
        let mut tx = env.tm.begin(0);
        let mut ctx = TxContext::new(&env.disk, &mut tx);
        // Scratch images are never committed, so each transaction must
        // re-reserve the metadata region (blocks 0..16 hold the
        // superblock, both bitmap blocks, and the tree roots).
        Allocator::mark_blocks_used(&mut ctx, env.bg.bg_block_bitmap, 0, 16).unwrap();
        let mut inode = fresh_inode(3);
        inode.compression_algo = 2;
        let before = env.blocks
            - Allocator::count_free_blocks(&mut ctx, env.bg.bg_block_bitmap, env.blocks).unwrap();
        let t = Instant::now();
        FileManager::write_file(&mut ctx, &env.bg, bpg, 0, &cctx, &mut inode, 0, &corpus).unwrap();
        let el = t.elapsed().as_secs_f64();
        let after = env.blocks
            - Allocator::count_free_blocks(&mut ctx, env.bg.bg_block_bitmap, env.blocks).unwrap();
        let consumed = after - before;
        println!(
            "    level {:>2}: ratio {:>5.2}x   write {:>7.1} MiB/s",
            level,
            logical_blocks as f64 / consumed as f64,
            corpus_len as f64 / (1024.0 * 1024.0) / el
        );
        drop(ctx);
        drop(tx);
    }
    lionfs_core::fs::compression::set_zstd_level(args.zstd_level);
}

/// Mixed-compressibility corpus generator (shared with the unit tests'
/// semantics; duplicated here so the tool is self-contained).
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
                out.extend_from_slice(dict[(next() % 16) as usize]);
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

/// RAID pool benchmark (Phase 2): the parity cost of RAID5/6 lives in
/// the COMMIT path (journal writes + applying every dirty block through
/// the RAID engine), not in the tx-buffered write loop. This group
/// therefore measures, separately:
///   1. raid-write-fresh : tx-buffered write pass (I/O core cost)
///   2. raid-commit      : TransactionManager::commit -- journal + fsync
///                         + per-block apply through write_block_parity
///   3. raid-read-back   : 64 KiB sequential reads in a NEW transaction
///                         (post-commit: every read hits the pool)
/// and reports the Phase 2 parity alignment counters across the commit.
fn run_raid_group(env: &mut Env, with_checksums: bool) -> Vec<ResultLine> {
    use lionfs_core::transaction::transaction::TxContext;
    let mut out = Vec::new();
    let bpg = env.blocks as u32;
    let cctx = BlockCipherContext::none();
    let unit_blocks: u64 = 16; // 64 KiB per call, matching the plan profile
    let units = REGION_BLOCKS / unit_blocks;
    let unit_bytes = unit_blocks * BLOCK_SIZE as u64;
    let payload = |u: u64| {
        let mut v = Vec::with_capacity(unit_blocks as usize * BLOCK_SIZE);
        for b in 0..unit_blocks {
            let i = u * unit_blocks + b;
            v.extend_from_slice(&vec![(i % 251) as u8; BLOCK_SIZE]);
        }
        v
    };

    let mut inode;
    {
        let mut tx = env.tm.begin(0);
        {
            let mut ctx = TxContext::new(&env.disk, &mut tx);
            Allocator::mark_blocks_used(&mut ctx, env.bg.bg_block_bitmap, 0, 16).unwrap();
            if with_checksums && env.checksum_tree_root != 0 {
                lionfs_core::integrity::checksum_tree::ChecksumTree::init_empty(
                    &mut ctx,
                    env.checksum_tree_root,
                )
                .unwrap();
            }
            let mut ino = fresh_inode(2);
            let t = Instant::now();
            for u in 0..units {
                FileManager::write_file(
                    &mut ctx,
                    &env.bg,
                    bpg,
                    env.checksum_tree_root,
                    &cctx,
                    &mut ino,
                    u * unit_bytes,
                    &payload(u),
                )
                .unwrap();
            }
            let el = t.elapsed().as_secs_f64();
            let (frag_inline, frag_spill) = count_extents(&mut ctx, &ino);
            out.push(ResultLine {
                pattern: "raid-write-fresh",
                mib_per_s: (REGION_BLOCKS * BLOCK_SIZE as u64) as f64 / (1024.0 * 1024.0) / el,
                ops_per_s: units as f64 / el,
                bytes: REGION_BLOCKS * BLOCK_SIZE as u64,
                secs: el,
                extents: frag_inline,
                spill_entries: frag_spill,
                spilled: ino.spill_extent_root != 0,
                checksums: with_checksums && env.checksum_tree_root != 0,
            });
            inode = ino;
        }
        // Commit: journal + fsync + RAID apply (the parity cost).
        lionfs_core::debug::stats::reset_parity_counters();
        let t = Instant::now();
        env.tm.commit(&env.disk, &env.sb, &tx).unwrap();
        let el = t.elapsed().as_secs_f64();
        eprintln!(
            "  parity: {}",
            lionfs_core::debug::stats::parity_alignment_report()
        );
        out.push(ResultLine {
            pattern: "raid-commit",
            mib_per_s: (REGION_BLOCKS * BLOCK_SIZE as u64) as f64 / (1024.0 * 1024.0) / el,
            ops_per_s: 1.0 / el,
            bytes: REGION_BLOCKS * BLOCK_SIZE as u64,
            secs: el,
            extents: 0,
            spill_entries: 0,
            spilled: false,
            checksums: with_checksums && env.checksum_tree_root != 0,
        });
    }

    // Read-back in a fresh transaction: every read goes to the pool.
    {
        let mut tx = env.tm.begin(0);
        let mut ctx = TxContext::new(&env.disk, &mut tx);
        let t = Instant::now();
        let mut verified = 0usize;
        for u in 0..units {
            let got = FileManager::read_file(
                &mut ctx,
                env.checksum_tree_root,
                0,
                &cctx,
                &mut inode,
                u * unit_bytes,
                unit_bytes,
            )
            .unwrap();
            if got == payload(u) {
                verified += 1;
            }
        }
        let el = t.elapsed().as_secs_f64();
        assert_eq!(
            verified, units as usize,
            "read-back must round-trip every unit"
        );
        out.push(ResultLine {
            pattern: "raid-read-back",
            mib_per_s: (REGION_BLOCKS * BLOCK_SIZE as u64) as f64 / (1024.0 * 1024.0) / el,
            ops_per_s: units as f64 / el,
            bytes: REGION_BLOCKS * BLOCK_SIZE as u64,
            secs: el,
            extents: inode.extent_count as u64,
            spill_entries: 0,
            spilled: inode.spill_extent_root != 0,
            checksums: with_checksums && env.checksum_tree_root != 0,
        });
    }

    // Random 4 KiB overwrites + commit: the plan's random-write parity
    // profile. All blocks already exist (phase 1 wrote them), so every
    // write is a parity RMW on an existing stripe row -- exactly what
    // the incremental path targets.
    {
        let mut tx = env.tm.begin(0);
        {
            let mut ctx = TxContext::new(&env.disk, &mut tx);
            let mut rng = Prng::new(0x5eed_9999);
            let block = |i: u64| vec![(i % 251) as u8; BLOCK_SIZE];
            let t = Instant::now();
            for _ in 0..REGION_BLOCKS {
                let target = (rng.next() % REGION_BLOCKS as u64) * BLOCK_SIZE as u64;
                FileManager::write_file(
                    &mut ctx,
                    &env.bg,
                    bpg,
                    env.checksum_tree_root,
                    &cctx,
                    &mut inode,
                    target,
                    &block(target / BLOCK_SIZE as u64),
                )
                .unwrap();
            }
            let el = t.elapsed().as_secs_f64();
            out.push(ResultLine {
                pattern: "raid-rand4k-write",
                mib_per_s: (REGION_BLOCKS * BLOCK_SIZE as u64) as f64 / (1024.0 * 1024.0) / el,
                ops_per_s: REGION_BLOCKS as f64 / el,
                bytes: REGION_BLOCKS * BLOCK_SIZE as u64,
                secs: el,
                extents: inode.extent_count as u64,
                spill_entries: 0,
                spilled: inode.spill_extent_root != 0,
                checksums: with_checksums && env.checksum_tree_root != 0,
            });
        }
        lionfs_core::debug::stats::reset_parity_counters();
        let t = Instant::now();
        env.tm.commit(&env.disk, &env.sb, &tx).unwrap();
        let el = t.elapsed().as_secs_f64();
        eprintln!(
            "  parity (rand): {}",
            lionfs_core::debug::stats::parity_alignment_report()
        );
        out.push(ResultLine {
            pattern: "raid-rand4k-commit",
            mib_per_s: (REGION_BLOCKS * BLOCK_SIZE as u64) as f64 / (1024.0 * 1024.0) / el,
            ops_per_s: 1.0 / el,
            bytes: REGION_BLOCKS * BLOCK_SIZE as u64,
            secs: el,
            extents: 0,
            spill_entries: 0,
            spilled: false,
            checksums: with_checksums && env.checksum_tree_root != 0,
        });
    }
    out
}

/// Random 4 KiB group. rand4k-read: untimed prefill of the region, then
/// random reads until budget (same tx + inode). rand4k-write: fresh
/// transaction and inode, random writes until budget (first ops
/// allocate, the rest are RMW overwrites, matching fio's fresh-file
/// randwrite profile).
fn run_rand_group(env: &mut Env, secs: f64, with_checksums: bool) -> Vec<ResultLine> {
    let mut out = Vec::new();
    let bpg = env.blocks as u32;
    let cctx = BlockCipherContext::none();
    let block = |i: u64| vec![(i % 251) as u8; BLOCK_SIZE];

    // rand4k-read
    {
        let mut tx = env.tm.begin(0);
        let mut ctx = TxContext::new(&env.disk, &mut tx);
        Allocator::mark_blocks_used(&mut ctx, env.bg.bg_block_bitmap, 0, 16).unwrap();
        if with_checksums && env.checksum_tree_root != 0 {
            lionfs_core::integrity::checksum_tree::ChecksumTree::init_empty(
                &mut ctx,
                env.checksum_tree_root,
            )
            .unwrap();
        }
        let mut inode = fresh_inode(2);
        for i in 0..REGION_BLOCKS {
            FileManager::write_file(
                &mut ctx,
                &env.bg,
                bpg,
                env.checksum_tree_root,
                &cctx,
                &mut inode,
                i * BLOCK_SIZE as u64,
                &block(i),
            )
            .unwrap();
        }
        let (frag_inline, frag_spill) = count_extents(&mut ctx, &inode);
        let mut rng = Prng::new(0x5eed_1234);
        let t = Instant::now();
        let budget = Duration::from_secs_f64(secs);
        let mut ops = 0u64;
        while t.elapsed() < budget {
            let target = (rng.next() % REGION_BLOCKS as u64) * BLOCK_SIZE as u64;
            let _ = FileManager::read_file(
                &mut ctx,
                env.checksum_tree_root,
                0,
                &cctx,
                &mut inode,
                target,
                BLOCK_SIZE as u64,
            )
            .unwrap();
            ops += 1;
        }
        let el = t.elapsed().as_secs_f64();
        let bytes = ops * BLOCK_SIZE as u64;
        out.push(ResultLine {
            pattern: "rand4k-read",
            mib_per_s: bytes as f64 / (1024.0 * 1024.0) / el,
            ops_per_s: ops as f64 / el,
            bytes,
            secs: el,
            extents: frag_inline,
            spill_entries: frag_spill,
            spilled: inode.spill_extent_root != 0,
            checksums: with_checksums && env.checksum_tree_root != 0,
        });
    }

    // rand4k-write (fresh inode: allocation + later RMW, like fio on a
    // fresh file)
    {
        let mut tx = env.tm.begin(0);
        let mut ctx = TxContext::new(&env.disk, &mut tx);
        Allocator::mark_blocks_used(&mut ctx, env.bg.bg_block_bitmap, 0, 16).unwrap();
        if with_checksums && env.checksum_tree_root != 0 {
            lionfs_core::integrity::checksum_tree::ChecksumTree::init_empty(
                &mut ctx,
                env.checksum_tree_root,
            )
            .unwrap();
        }
        let mut inode = fresh_inode(2);
        let mut rng = Prng::new(0x5eed_1234);
        let t = Instant::now();
        let budget = Duration::from_secs_f64(secs);
        let mut ops = 0u64;
        while t.elapsed() < budget {
            let target = (rng.next() % REGION_BLOCKS as u64) * BLOCK_SIZE as u64;
            FileManager::write_file(
                &mut ctx,
                &env.bg,
                bpg,
                env.checksum_tree_root,
                &cctx,
                &mut inode,
                target,
                &block(target / BLOCK_SIZE as u64),
            )
            .unwrap();
            ops += 1;
        }
        let el = t.elapsed().as_secs_f64();
        let bytes = ops * BLOCK_SIZE as u64;
        let (frag_inline, frag_spill) = count_extents(&mut ctx, &inode);
        out.push(ResultLine {
            pattern: "rand4k-write",
            mib_per_s: bytes as f64 / (1024.0 * 1024.0) / el,
            ops_per_s: ops as f64 / el,
            bytes,
            secs: el,
            extents: frag_inline,
            spill_entries: frag_spill,
            spilled: inode.spill_extent_root != 0,
            checksums: with_checksums && env.checksum_tree_root != 0,
        });
    }

    out
}

fn main() {
    let args = parse_args();
    let secs = args.secs;

    let mut env = build_env(&args);
    let mut results = Vec::new();
    if args.compress {
        run_compress_group(&mut env, &args);
        let _ = fs::remove_file(&env.path);
        for p in &env.paths {
            let _ = fs::remove_file(p);
        }
        return;
    }
    if args.profile != "single" {
        results.extend(run_raid_group(&mut env, args.checksums));
    } else {
        results.extend(run_seq_group(
            &mut env,
            secs,
            args.checksums,
            K4,
            "seq4k-write",
        ));
        results.extend(run_seq_group(
            &mut env,
            secs,
            args.checksums,
            K64,
            "seq64k-write",
        ));
        results.extend(run_rand_group(&mut env, secs, args.checksums));
    }

    if args.json {
        println!("[");
        for (i, r) in results.iter().enumerate() {
            println!("  {{\"pattern\": \"{}\", \"mib_per_s\": {:.2}, \"ops_per_s\": {:.0}, \"bytes\": {}, \"secs\": {:.3}, \"extents\": {}, \"spill_entries\": {}, \"spilled\": {}, \"checksums\": {}}}{}",
                r.pattern, r.mib_per_s, r.ops_per_s, r.bytes, r.secs, r.extents, r.spill_entries, r.spilled, r.checksums,
                if i + 1 < results.len() { "," } else { "" });
        }
        println!("]");
    } else {
        println!("lfs_ioperf: in-process LionFS I/O-core benchmark");
        println!("  image        : {} (scratch)", env.path);
        println!(
            "  region       : {} blocks ({} MiB), 4 KiB units",
            REGION_BLOCKS,
            REGION_BLOCKS * BLOCK_SIZE as u64 / (1024 * 1024)
        );
        println!("  pattern note : user-space CPU cost of the I/O path; NOT comparable");
        println!("                 to fio-on-mount numbers (no FUSE, no page cache, no syscalls)");
        println!(
            "  checksums    : {}",
            if args.checksums {
                "on (XxHash64 per block, checksum tree)"
            } else {
                "off"
            }
        );
        println!("  harness      : tx-buffered (like the FUSE path between fsyncs), one");
        println!("                 tx per group; images are scratch and never committed");
        println!();
        println!(
            "  {:<18} {:>10} {:>10} {:>11} {:>8} {:>9}",
            "pattern", "MiB/s", "ops/s", "bytes", "extents", "fragments"
        );
        for r in &results {
            println!(
                "  {:<18} {:>10.1} {:>10.0} {:>11} {:>8} {:>9}",
                r.pattern,
                r.mib_per_s,
                r.ops_per_s,
                r.bytes,
                r.extents,
                r.extents + r.spill_entries
            );
        }
    }

    let _ = fs::remove_file(&env.path);
}
