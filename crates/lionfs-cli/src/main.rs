//! `lion` — the unified LionFS front-end (LionFS 8.0).
//!
//! Built-ins:
//!   lion info              — version, edition, and the capability map
//!   lion guide             — every tool the merge shipped, one screen
//!   lion mount <img> <dir> — FUSE mount through the library API
//!
//! Everything else dispatches to the specialized `lfs_*` tool of the
//! same name (the git-subcommand model): `lion snapshot create IMG 7`
//! runs `lfs_snapshot create IMG 7`, `lion timetravel list IMG` runs
//! `lfs_timetravel list IMG`, and so on. Binaries are resolved from
//! the directory of this executable first, then from `PATH`.

use std::path::PathBuf;
use std::process::Command;

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// Find `lfs_<name>` next to this executable, then on PATH.
fn resolve_tool(name: &str) -> Option<PathBuf> {
    let tool = format!("lfs_{name}");
    if let Some(dir) = exe_dir() {
        let candidate = dir.join(&tool);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // PATH search.
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = PathBuf::from(dir).join(&tool);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn dispatch(name: &str, args: &[String]) -> i32 {
    match resolve_tool(name) {
        Some(tool) => {
            let status = Command::new(&tool)
                .args(args)
                .status()
                .map(|s| s.code().unwrap_or(1))
                .unwrap_or_else(|e| {
                    eprintln!("lion: cannot execute {}: {e}", tool.display());
                    1
                });
            status
        }
        None => {
            eprintln!("lion: no built-in or tool named {name:?}");
            eprintln!("run `lion guide` for the capability map");
            1
        }
    }
}

const GUIDE: &str = "\
LionFS 8.0 'Unified' — the LFS x HFS merge. One filesystem:
line-rate local engine + distributed, self-healing, time-aware cluster plane.

LOCAL ENGINE (from LionFS LFS):
  mkfs_lfs        format an image or device pool (RAID 0/1/5/6/10)
  mount_lfs       FUSE mount (Linux/macFUSE)
  lfs_snapshot    create/list/delete/verify snapshots (O(1) metadata)
  lfs_clone       reflink whole-file clones
  lfs_gc          copy-GC planner (Rosenblum-Ousterhout cost/benefit)
  lfs_retention   GFS retention policy engine
  lfs_scrub       checksum sweep + RAID bit-rot self-healing
  lfs_migrate     foreign-filesystem import (tar-stream, SHA-256 manifest)
  lfs_simulate    deterministic full-stack crash simulator
  lfs_ioperf      p50/p99/p999 per-call latency reporter
  lfs_smpbench    measured SMP scaling
  lfs_guardian    ransomware / drive-risk / workload advisories
  lfs_zns         ZNS zone-append placement simulation
  lfs_palinfo     platform abstraction layer self-test

CLUSTER PLANE (merged from HFS):
  lfs_cluster     the showcase: Raft failover, CDC dedup, convergent
                  encryption, RS self-healing, time travel — one command
  lfs_timetravel  path-based time travel over the snapshot timeline:
                  list / stat / cat / ls / diff (times or @snapshot ids)

MERGED CAPABILITIES (8.0 integrations):
  lfs_dedupe      FastCDC dedup analysis with domain-scoped convergent
                  identities (image files, host files, or the demo)
  lfs_raid        RS erasure-coded fragment volumes: encode any file
                  into n fragments, rebuild from any k
  lfs_verify      image + fragment integrity verification
  lfs_predict     pool MTTDL / availability / write-amplification
                  (corrected CTMC models) + Prometheus exposition

BUILT-INS:
  lion info       version + capability map
  lion mount      FUSE mount through the library API
  lion guide      this screen

Dispatch: `lion <tool-suffix> ...` runs `lfs_<tool-suffix> ...`
(e.g. `lion snapshot create IMG 7`, `lion timetravel list IMG`).";

fn cmd_info() -> i32 {
    println!("LionFS {} ({})", lionfs_core::VERSION, lionfs_core::EDITION);
    println!("engine:    lionfs_core {} — local plane (LFS lineage)", lionfs_core::VERSION);
    println!("cluster:   lionfs_cluster {} — distributed plane (HFS merge)", lionfs_cluster::VERSION);
    println!();
    println!("capability matrix:");
    println!("  line-rate local I/O        FUSE / PAL / io_uring / RAID 0-10   [LFS]");
    println!("  B-epsilon extent index     write-optimized, path-copy CoW      [LFS]");
    println!("  O(1) snapshots             frozen roots, birth generations     [LFS]");
    println!("  path-based time travel     resolve(path, t), diff, timeline    [HFS->LFS 8.0]");
    println!("  Raft consensus             leader election, failover, converge [HFS]");
    println!("  CRDT namespace             conflict-free replicated state      [HFS]");
    println!("  CDC dedup                  FastCDC + domain-scoped identities   [HFS+LFS]");
    println!("  convergent encryption      dedup-compatible AEAD, per-domain    [HFS]");
    println!("  Reed-Solomon EC            dual independent GF(256) codecs      [HFS+LFS]");
    println!("  WAL discipline             journal + mount-time checkpoint      [LFS+HFS]");
    println!("  reliability math           MTTDL (CTMC), WA = 1/(1-u)           [HFS->LFS 8.0]");
    println!("  Guardian autonomy          Weibull hazard, ransomware entropy   [LFS]");
    0
}

fn cmd_mount(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("Usage: lion mount <image> <mountpoint> [extra devices...]");
        return 1;
    }
    #[cfg(unix)]
    {
        use lionfs_core::api::options::LfsOptions;
        use lionfs_core::mount::mount as mount_mod;
        let image = &args[0];
        let mnt = &args[1];
        let options = LfsOptions {
            device_path: image.clone(),
            extra_devices: args[2..].to_vec(),
            read_only: false,
            default_compression: 0,
            default_encryption: 0,
        };
        let config = lionfs_core::common::config::MountConfig::default();
        match mount_mod::mount_and_serve(options, &config, mnt) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("lion mount: {e}");
                2
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        eprintln!("lion mount: FUSE mounting requires a Unix platform");
        2
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        println!("{}", GUIDE);
        std::process::exit(0);
    }
    let code = match args[1].as_str() {
        "info" => cmd_info(),
        "guide" | "help" | "--help" | "-h" => {
            println!("{}", GUIDE);
            0
        }
        "mount" => cmd_mount(&args[2..]),
        other => dispatch(other, &args[2..]),
    };
    std::process::exit(code);
}
