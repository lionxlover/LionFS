//! `lfs_smpbench` -- measured SMP scalability of the LionFS data paths
//! (Phase 9 reads, Phase 10 writes).
//!
//! Read mode (default): N worker threads over ONE shared `Disk`
//! (positioned pread via the PAL, so no seek races), each with its own
//! transaction context and its own subset of the image's files, doing
//! full sequential (default) or random-4KiB reads through the real
//! `FileManager::read_file` path -- tree descents, checksum lookups,
//! block reads, all with a SHARED node cache (contention included).
//!
//! Write mode (`--write buffered|durable`, Phase 10): N worker threads
//! over ONE shared mounted `LionFS` driven through the real `VfsOps`
//! surface -- exactly what a library consumer, the C ABI, or a
//! multi-threaded bridge gets. Each thread owns one file (per-inode
//! write gates never contend), writes 64-KiB sequential calls at a
//! rotating offset within an 8-MiB window (so overwrites, RMW fetches,
//! and page-cache re-dirty are all exercised, and no image fills up).
//! * `buffered` measures the write-back INTAKE path (what write(2)
//!   returns to the app; flushes only trigger on the 32-MiB dirty
//!   threshold inside the window -- the lazy group commit);
//! * `durable` calls fsync after every 64 KiB (the flush + journal
//!   commit + sync pipeline, end to end).
//! The untimed epilogue (fsync + destroy) flushes every residue.
//!
//! Methodology (honest, interleaved): a single-threaded BASELINE phase
//! runs first for T seconds, then the N-thread phase runs for T
//! seconds in the SAME process on the SAME image; the scaling factor
//! is the ratio of aggregate throughputs.
//!
//! Usage:
//!   lfs_smpbench <image> [--jobs N] [--seconds T] [--rw seq|rand] [--no-cache]
//!   lfs_smpbench <image> --write buffered|durable [--jobs N] [--seconds T]

use lionfs_core::cache::node_cache::NodeCache;
use lionfs_core::disk::block_io::Disk;
use lionfs_core::file::writer::FileManager;
use lionfs_core::inode::tree::INODE_TREE_NODE_TYPE;
use lionfs_core::ondisk::serialization::{Inode, Superblock, BLOCK_SIZE, LIONFS_MAGIC};
use lionfs_core::security::block_cipher::BlockCipherContext;
use lionfs_core::transaction::transaction::{Transaction, TxContext};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Args {
    image: String,
    jobs: usize,
    seconds: u64,
    rand: bool,
    shared_cache: bool,
    write: Option<String>,
    /// Durable mode: fsync after this many KiB (default 64, the
    /// per-call pathological case; 1024+ shows the group-commit
    /// amortization).
    fsync_every_kib: u64,
    /// Phase 10: read through the mounted vfs path (&self `read`) --
    /// inode cache + page cache + committed reads, one file per
    /// thread. This is the parallel-READ surface the `&self` VfsOps
    /// refactor unlocked (3.3 serialized every op on `&mut self`).
    read_vfs: bool,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() < 2 {
        eprintln!(
            "Usage: lfs_smpbench <image> [--jobs N] [--seconds T] [--rw seq|rand] [--no-cache]"
        );
        std::process::exit(1);
    }
    let mut a = Args {
        image: raw[1].clone(),
        jobs: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
        seconds: 5,
        rand: false,
        shared_cache: true,
        write: None,
        fsync_every_kib: 64,
        read_vfs: false,
    };
    let mut i = 2;
    while i < raw.len() {
        match raw[i].as_str() {
            "--jobs" => {
                a.jobs = raw.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(a.jobs);
                i += 2;
            }
            "--seconds" => {
                a.seconds = raw.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(a.seconds);
                i += 2;
            }
            "--rw" => {
                a.rand = raw.get(i + 1).map(|s| s == "rand").unwrap_or(false);
                i += 2;
            }
            "--no-cache" => {
                a.shared_cache = false;
                i += 1;
            }
            "--read-vfs" => {
                a.read_vfs = true;
                i += 1;
            }
            "--fsync-every" => {
                a.fsync_every_kib = raw
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(a.fsync_every_kib)
                    .max(64);
                i += 2;
            }
            "--write" => {
                a.write = raw.get(i + 1).cloned();
                if a.write.as_deref() != Some("buffered") && a.write.as_deref() != Some("durable") {
                    eprintln!("--write needs 'buffered' or 'durable'");
                    std::process::exit(1);
                }
                i += 2;
            }
            other => {
                eprintln!("unknown flag: {other}");
                std::process::exit(1);
            }
        }
    }
    a
}

fn read_sb(disk: &Disk) -> Superblock {
    let mut buf = [0u8; BLOCK_SIZE];
    disk.read_block(0, &mut buf).expect("read superblock");
    let sb: Superblock = *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
    assert_eq!(sb.magic, LIONFS_MAGIC, "not a LionFS image");
    sb
}

/// Enumerate the image's regular, uncompressed files (the readable
/// data set for the benchmark).
fn list_files(disk: &Disk, sb: &Superblock) -> Vec<Inode> {
    let mut tx = Transaction::new(0, 0);
    let mut ctx = TxContext::new(disk, &mut tx);
    let tree =
        lionfs_core::btree::tree::BTree::<u64, Inode>::new(sb.inode_tree_root, INODE_TREE_NODE_TYPE);
    match tree.iter_all(&mut ctx) {
        Ok(all) => all
            .into_iter()
            .map(|(_, ino)| ino)
            .filter(|i| i.size > 0 && i.compression_algo == 0)
            .collect(),
        Err(e) => {
            eprintln!("ERROR: inode enumeration failed: {e}");
            std::process::exit(1);
        }
    }
}

/// One read slice: (inode, byte start, byte length) -- disjoint across
/// workers by construction.
struct Slice {
    inode: Inode,
    start: u64,
    len: u64,
}

/// One worker: reads its disjoint slices through a private transaction
/// context until the deadline, returning bytes read.
fn worker(
    disk: &Disk,
    sb: &Superblock,
    cache: Option<&NodeCache>,
    slices: Vec<Slice>,
    rand: bool,
    deadline: Instant,
) -> u64 {
    let mut bytes = 0u64;
    let cctx = BlockCipherContext {
        compression_algo: 0,
        encryption_algo: 0,
        key: None,
        crypto_tree_root: 0,
    };
    let mut rng_state: u64 = 0x9E3779B97F4A7C15u64.wrapping_add(slices.len() as u64);
    let mut next_rand = || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        rng_state
    };
    'outer: while Instant::now() < deadline {
        for sl in &slices {
            if Instant::now() >= deadline {
                break 'outer;
            }
            let mut inode = sl.inode;
            let mut tx = Transaction::new(0, 0);
            let mut ctx = match cache {
                Some(c) => TxContext::with_cache(disk, &mut tx, c),
                None => TxContext::new(disk, &mut tx),
            };
            if rand {
                // Random 4 KiB-aligned reads within THIS worker's slice.
                let first = sl.start / BLOCK_SIZE as u64;
                let count = sl.len / BLOCK_SIZE as u64;
                if count == 0 {
                    continue;
                }
                for _ in 0..64 {
                    if Instant::now() >= deadline {
                        break 'outer;
                    }
                    let lb = first + next_rand() % count;
                    match FileManager::read_file(
                        &mut ctx,
                        sb.checksum_tree_root,
                        sb.bad_blocks_root,
                        &cctx,
                        &mut inode,
                        lb * BLOCK_SIZE as u64,
                        BLOCK_SIZE as u64,
                    ) {
                        Ok(_) => bytes += BLOCK_SIZE as u64,
                        Err(_) => continue,
                    }
                }
            } else {
                // Sequential pass over THIS worker's slice.
                match FileManager::read_file(
                    &mut ctx,
                    sb.checksum_tree_root,
                    sb.bad_blocks_root,
                    &cctx,
                    &mut inode,
                    sl.start,
                    sl.len,
                ) {
                    Ok(data) => bytes += data.len() as u64,
                    Err(_) => continue,
                }
            }
        }
    }
    bytes
}

/// Run `jobs` threads for `secs` over the shared disk; returns
/// (aggregate bytes, elapsed).
fn phase(
    disk: &Arc<Disk>,
    sb: &Superblock,
    files: &[Inode],
    cache: Option<&Arc<NodeCache>>,
    jobs: usize,
    secs: u64,
    rand: bool,
) -> (u64, Duration) {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let disk_ref: &Disk = disk;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..jobs)
            .map(|t| {
                // Every file is cut into `jobs` contiguous byte ranges;
                // worker t takes range t of every file -- disjoint by
                // construction whether there is one file or thousands.
                let slices: Vec<Slice> = files
                    .iter()
                    .filter(|f| f.size >= BLOCK_SIZE as u64)
                    .map(|f| {
                        let per = (f.size / jobs as u64 / BLOCK_SIZE as u64)
                            .max(1)
                            * BLOCK_SIZE as u64;
                        let start = (t as u64) * per;
                        let len = per.min(f.size.saturating_sub(start));
                        Slice {
                            inode: *f,
                            start,
                            len,
                        }
                    })
                    .filter(|sl| sl.len > 0)
                    .collect();
                let cache = cache;
                let rand = rand;
                let deadline = deadline;
                std::thread::Builder::new()
                    .stack_size(8 << 20)
                    .spawn_scoped(scope, move || {
                        worker(disk_ref, sb, cache.map(|c| &**c), slices, rand, deadline)
                    })
                    .expect("spawn worker")
            })
            .collect();
        let mut total = 0u64;
        for h in handles {
            total += h.join().expect("worker panicked");
        }
        (total, start.elapsed())
    })
}

/// Populate the image through the REAL mounted-vfs path (create +
/// write + commit), so the benchmark reads a genuinely formatted,
/// genuinely written filesystem. Must be run on a freshly mkfs'd
/// image. Deterministic pseudorandom content (xorshift), so repeated
/// runs read the same bytes.
fn prepare(image: &str, files: usize, mib_each: usize) {
    let disk = Disk::open(image).expect("open image");
    let mut fs = lionfs_core::fs::filesystem::LionFS::new(disk, image.to_string())
        .expect("mount for prepare");
    use lionfs_core::vfs::{VfsCreate, VfsOps};
    let create = VfsCreate {
        mode: 0o100644,
        uid: 0,
        gid: 0,
    };
    let per_write = 256 * 1024usize;
    let file_len = mib_each * (1 << 20);
    for i in 0..files {
        let name = format!("bench_{i:03}.bin");
        let attr = fs.create(1, &name, &create).expect("create file");
        let ino = attr.ino;
        let mut state: u64 = 0x1234_5678_9ABC_DEF0u64.wrapping_add(i as u64);
        let mut offset = 0u64;
        let mut buf = vec![0u8; per_write];
        while offset < file_len as u64 {
            for chunk in buf.chunks_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                chunk.copy_from_slice(&state.to_le_bytes());
            }
            let n = per_write.min((file_len as u64 - offset) as usize);
            let wrote = fs.write(ino, offset, &buf[..n]).expect("write");
            assert_eq!(wrote, n as u32, "short write during prepare");
            offset += n as u64;
        }
        println!("  wrote {name} ({mib_each} MiB, ino {ino})");
    }
    fs.destroy();
    println!("prepare complete: {files} files x {mib_each} MiB on {image}");
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some("--prepare") {
        let image = raw.get(2).cloned().unwrap_or_default();
        let files: usize = raw.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
        let mib: usize = raw.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
        if image.is_empty() {
            eprintln!("usage: lfs_smpbench --prepare <image> <files> <mib_each>");
            std::process::exit(1);
        }
        prepare(&image, files, mib);
        return;
    }
    let args = parse_args();
    if args.write.is_some() {
        run_write_bench(&args);
        return;
    }
    if args.read_vfs {
        run_read_vfs_bench(&args);
        return;
    }
    let disk = Arc::new(Disk::open(&args.image).expect("open image"));
    let sb = read_sb(&disk);
    let files = list_files(&disk, &sb);
    let total_data: u64 = files.iter().map(|f| f.size).sum();
    if files.is_empty() {
        eprintln!("ERROR: image has no readable uncompressed files; write data first");
        std::process::exit(1);
    }
    let cache: Option<Arc<NodeCache>> = args.shared_cache.then(|| Arc::new(NodeCache::new(4096)));

    println!("LionFS SMP read benchmark (3.3/3.4)");
    println!(
        "image: {} | files: {} | data: {} MiB | jobs: {} | mode: {} | cache: {}",
        args.image,
        files.len(),
        total_data >> 20,
        args.jobs,
        if args.rand { "rand4k" } else { "sequential" },
        if args.shared_cache { "shared" } else { "off" }
    );
    println!("NOTE: this measures READ scaling; use --write for the Phase 10 write path.\n");

    // Interleaved same-process baseline: 1 job, then N jobs, equal time
    // windows, same image, same code path.
    let (base_bytes, base_dur) = phase(&disk, &sb, &files, cache.as_ref(), 1, args.seconds, args.rand);
    let base_mibps = base_bytes as f64 / base_dur.as_secs_f64() / (1 << 20) as f64;
    println!(
        "baseline  (1 job ): {:>8.2} MiB/s in {:.1}s",
        base_mibps,
        base_dur.as_secs_f64()
    );

    let (par_bytes, par_dur) =
        phase(&disk, &sb, &files, cache.as_ref(), args.jobs, args.seconds, args.rand);
    let par_mibps = par_bytes as f64 / par_dur.as_secs_f64() / (1 << 20) as f64;
    println!(
        "parallel  ({} jobs): {:>8.2} MiB/s in {:.1}s (aggregate)",
        args.jobs,
        par_mibps,
        par_dur.as_secs_f64()
    );

    let scaling = if base_mibps > 0.0 { par_mibps / base_mibps } else { 0.0 };
    println!(
        "\nscaling: {:.2}x at {} jobs ({:.0}% efficiency)",
        scaling,
        args.jobs,
        100.0 * scaling / args.jobs.max(1) as f64
    );
    println!(
        "json: {{\"jobs\": {}, \"baseline_mibps\": {:.3}, \"parallel_mibps\": {:.3}, \"scaling\": {:.3}}}",
        args.jobs, base_mibps, par_mibps, scaling
    );
}

// ---------------------------------------------------------------------------
// Phase 10 write benchmarks (real VfsOps path, one shared mount).
// ---------------------------------------------------------------------------

use lionfs_core::fs::filesystem::LionFS;
use lionfs_core::vfs::{VfsCreate, VfsOps};

const WB_IO: usize = 64 * 1024; // one write() call per iteration
const WB_WINDOW: u64 = 8 * 1024 * 1024; // rotating overwrite window per file

/// One writer thread: owns `file_t`, sequential 64-KiB writes at a
/// rotating offset, until the deadline. Returns bytes handed to
/// write(). `durable` adds an fsync per iteration (paid inside the
/// window -- that is the durability pipeline cost).
fn write_worker(
    fs: &Arc<LionFS>,
    t: usize,
    durable: bool,
    fsync_every_kib: u64,
    deadline: Instant,
) -> u64 {
    use lionfs_core::vfs::VfsSetAttr;
    let name = format!("wb_{t}.bin");
    // Fresh file per phase: truncate any residue from a prior phase so
    // the window starts clean.
    let attr = match fs.create(1, &name, &VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 }) {
        Ok(a) => a,
        Err(_) => match fs.lookup(1, &name) {
            Ok(a) => {
                let _ = fs.setattr(
                    a.ino,
                    &VfsSetAttr { size: Some(0), ..Default::default() },
                );
                a
            }
            Err(_) => return 0,
        },
    };
    let ino = attr.ino;
    let mut buf = vec![0u8; WB_IO];
    let mut state: u64 = 0x1234_5678_9ABC_DEF0u64 ^ (t as u64 + 1);
    let mut off: u64 = 0;
    let mut bytes = 0u64;
    let mut unsynced: u64 = 0;
    while Instant::now() < deadline {
        for chunk in buf.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        match fs.write(ino, off, &buf) {
            Ok(n) => {
                bytes += n as u64;
                unsynced += n as u64;
            }
            Err(_) => return bytes,
        }
        if durable && unsynced >= fsync_every_kib * 1024 {
            if fs.fsync(ino, false).is_err() {
                return bytes;
            }
            unsynced = 0;
        }
        off += WB_IO as u64;
        if off >= WB_WINDOW {
            off = 0; // rotate: overwrite path + page re-dirty
        }
    }
    bytes
}

fn write_phase(
    fs: &Arc<LionFS>,
    jobs: usize,
    secs: u64,
    durable: bool,
    fsync_every_kib: u64,
) -> (u64, Duration) {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..jobs)
            .map(|t| {
                let fs = Arc::clone(fs);
                let deadline = deadline;
                let fsync_every_kib = fsync_every_kib;
                std::thread::Builder::new()
                    .stack_size(8 << 20)
                    .spawn_scoped(scope, move || {
                        write_worker(&fs, t, durable, fsync_every_kib, deadline)
                    })
                    .expect("spawn writer")
            })
            .collect();
        let mut total = 0u64;
        for h in handles {
            total += h.join().expect("writer panicked");
        }
        (total, start.elapsed())
    })
}

fn run_write_bench(args: &Args) {
    let durable = args.write.as_deref() == Some("durable");
    // Validate the image looks like LionFS before mounting.
    {
        let disk = Disk::open(&args.image).expect("open image");
        let _sb = read_sb(&disk);
    }
    let fs = Arc::new(
        LionFS::new(Disk::open(&args.image).expect("reopen"), args.image.clone())
            .expect("mount"),
    );

    println!(
        "LionFS SMP write benchmark (3.4, Phase 10) -- real VfsOps path, one shared mount",
    );
    println!(
        "image: {} | jobs: {} | mode: {} | io: 64 KiB calls | window: 8 MiB rotating/file",
        args.image,
        args.jobs,
        if durable {
            format!("durable (fsync per {} KiB)", args.fsync_every_kib)
        } else {
            "buffered (write-back intake)".to_string()
        },
    );
    println!(
        "NOTE: buffered = intake throughput (flushes on the 32-MiB dirty threshold);\n      durable  = end-to-end (flush + journal commit + device sync).\n"
    );

    let (base_bytes, base_dur) =
        write_phase(&fs, 1, args.seconds, durable, args.fsync_every_kib);
    let base_mibps = base_bytes as f64 / base_dur.as_secs_f64() / (1 << 20) as f64;
    println!(
        "baseline  (1 job ): {:>8.2} MiB/s in {:.1}s",
        base_mibps,
        base_dur.as_secs_f64()
    );

    let (par_bytes, par_dur) =
        write_phase(&fs, args.jobs, args.seconds, durable, args.fsync_every_kib);
    let par_mibps = par_bytes as f64 / par_dur.as_secs_f64() / (1 << 20) as f64;
    println!(
        "parallel  ({} jobs): {:>8.2} MiB/s in {:.1}s (aggregate)",
        args.jobs,
        par_mibps,
        par_dur.as_secs_f64()
    );

    let scaling = if base_mibps > 0.0 { par_mibps / base_mibps } else { 0.0 };
    println!(
        "\nscaling: {:.2}x at {} jobs ({:.0}% efficiency)",
        scaling,
        args.jobs,
        100.0 * scaling / args.jobs.max(1) as f64
    );
    println!(
        "json: {{\"jobs\": {}, \"mode\": \"{}\", \"baseline_mibps\": {:.3}, \"parallel_mibps\": {:.3}, \"scaling\": {:.3}}}",
        args.jobs,
        if durable {
            format!("durable-{}k", args.fsync_every_kib)
        } else {
            "buffered".to_string()
        },
        base_mibps,
        par_mibps,
        scaling
    );

    // Untimed epilogue: full durability for everything written.
    for t in 0..args.jobs.max(1) {
        if let Ok(a) = fs.lookup(1, &format!("wb_{t}.bin")) {
            let _ = fs.fsync(a.ino, false);
        }
    }
}


// ---------------------------------------------------------------------------
// Phase 10 vfs-path read benchmark: N threads, one file each, through
// the &self `VfsOps::read` surface (3.3 serialized every op).
// ---------------------------------------------------------------------------

const RV_IO: u32 = 64 * 1024;

fn vfs_read_worker(fs: &Arc<LionFS>, t: usize, deadline: Instant) -> u64 {
    let name = format!("bench_{t:03}.bin");
    let attr = match fs.lookup(1, &name) {
        Ok(a) => a,
        Err(_) => return 0,
    };
    let size = attr.size;
    if size == 0 {
        return 0;
    }
    let mut off: u64 = 0;
    let mut bytes = 0u64;
    while Instant::now() < deadline {
        match fs.read(attr.ino, off, RV_IO) {
            Ok(data) => {
                bytes += data.len() as u64;
                if data.len() < RV_IO as usize {
                    off = 0;
                } else {
                    off += RV_IO as u64;
                    if off >= size {
                        off = 0;
                    }
                }
            }
            Err(_) => return bytes,
        }
    }
    bytes
}

fn vfs_read_phase(fs: &Arc<LionFS>, jobs: usize, secs: u64) -> (u64, Duration) {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..jobs)
            .map(|t| {
                let fs = Arc::clone(fs);
                let deadline = deadline;
                std::thread::Builder::new()
                    .stack_size(8 << 20)
                    .spawn_scoped(scope, move || vfs_read_worker(&fs, t, deadline))
                    .expect("spawn reader")
            })
            .collect();
        let mut total = 0u64;
        for h in handles {
            total += h.join().expect("reader panicked");
        }
        (total, start.elapsed())
    })
}

fn run_read_vfs_bench(args: &Args) {
    // Fresh image with one file per worker, written through the real
    // vfs path (prepare semantics, inline).
    let img = format!("/tmp/vfsread_{}.img", std::process::id());
    let _ = std::fs::remove_file(&img);
    {
        let mkfs = std::process::Command::new(std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("mkfs_lfs")))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mkfs_lfs".into()))
        .arg(&img).arg("96").output();
        match mkfs {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("failed to run mkfs_lfs for {}", img);
                std::process::exit(1);
            }
        }
    }
    let fs = Arc::new(
        LionFS::new(Disk::open(&img).expect("open"), img.clone()).expect("mount"),
    );
    let per_file: u64 = 8 * 1024 * 1024;
    for t in 0..args.jobs.max(1) {
        let name = format!("bench_{t:03}.bin");
        let attr = fs
            .create(1, &name, &VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 })
            .expect("create");
        let mut off = 0u64;
        let buf = vec![(t % 251) as u8 + 1; 64 * 1024];
        while off < per_file {
            let n = fs.write(attr.ino, off, &buf).expect("write") as u64;
            off += n;
        }
        fs.fsync(attr.ino, false).expect("fsync");
    }
    println!("LionFS vfs READ benchmark (3.4, Phase 10) -- &self VfsOps path, one file per thread");
    println!("image: {} | jobs: {} | io: 64 KiB reads | files: 8 MiB each\n", img, args.jobs);

    let (base_bytes, base_dur) = vfs_read_phase(&fs, 1, args.seconds);
    let base_mibps = base_bytes as f64 / base_dur.as_secs_f64() / (1 << 20) as f64;
    println!(
        "baseline  (1 job ): {:>8.2} MiB/s in {:.1}s",
        base_mibps,
        base_dur.as_secs_f64()
    );
    let (par_bytes, par_dur) = vfs_read_phase(&fs, args.jobs, args.seconds);
    let par_mibps = par_bytes as f64 / par_dur.as_secs_f64() / (1 << 20) as f64;
    println!(
        "parallel  ({} jobs): {:>8.2} MiB/s in {:.1}s (aggregate)",
        args.jobs,
        par_mibps,
        par_dur.as_secs_f64()
    );
    let scaling = if base_mibps > 0.0 { par_mibps / base_mibps } else { 0.0 };
    println!(
        "\nscaling: {:.2}x at {} jobs ({:.0}% efficiency)",
        scaling,
        args.jobs,
        100.0 * scaling / args.jobs.max(1) as f64
    );
    println!(
        "json: {{\"jobs\": {}, \"mode\": \"read-vfs\", \"baseline_mibps\": {:.3}, \"parallel_mibps\": {:.3}, \"scaling\": {:.3}}}",
        args.jobs, base_mibps, par_mibps, scaling
    );
    let _ = std::fs::remove_file(&img);
}
