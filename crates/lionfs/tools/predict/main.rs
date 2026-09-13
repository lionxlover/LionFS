//! `lfs_predict` — pool reliability prediction for a LionFS image
//! (LionFS 8.0: the HFS-merge reliability math behind a real tool —
//! this was a JSON-banner stub through 7.1).
//!
//! Reads the image's superblock (profile + capacity accounting),
//! evaluates the corrected CTMC MTTDL model, the availability bound,
//! and the write-amplification models at the image's CURRENT
//! utilization, and emits both a human summary and Prometheus text.
//!
//! Usage:
//!   lfs_predict <image> [--devices N] [--mttf-hours F] [--mttr-hours R]
//!                [--prometheus]

use lionfs_core::disk::block_io::Disk;
use lionfs_core::fs::stat::compute_stats;
use lionfs_core::guardian::reliability::{
    assess_pool, format_hours, render_prometheus, write_amplification,
};
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
use lionfs_core::pool::raid::RaidProfile;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("LionFS reliability predictor {} ({})", lionfs_core::VERSION, lionfs_core::EDITION);
        eprintln!("Usage: lfs_predict <image> [--devices N] [--mttf-hours F] [--mttr-hours R] [--prometheus]");
        eprintln!();
        eprintln!("  --devices N       device count of the pool (default: profile minimum)");
        eprintln!("  --mttf-hours F    per-device mean time to failure (default 1000000 = spec reference)");
        eprintln!("  --mttr-hours R    mean time to repair a failed device (default 4 = spec reference)");
        eprintln!("  --prometheus      emit Prometheus text exposition instead of the human summary");
        std::process::exit(1);
    }
    let image = &args[1];
    let mut devices: Option<u32> = None;
    let mut mttf: f64 = 1.0e6;
    let mut mttr: f64 = 4.0;
    let mut prometheus = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--devices" => {
                i += 1;
                devices = args.get(i).and_then(|s| s.parse().ok());
                if devices.is_none() {
                    eprintln!("ERROR: --devices needs a number");
                    std::process::exit(1);
                }
            }
            "--mttf-hours" => {
                i += 1;
                mttf = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(-1.0);
                if mttf <= 0.0 {
                    eprintln!("ERROR: --mttf-hours needs a positive number");
                    std::process::exit(1);
                }
            }
            "--mttr-hours" => {
                i += 1;
                mttr = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(-1.0);
                if mttr <= 0.0 {
                    eprintln!("ERROR: --mttr-hours needs a positive number");
                    std::process::exit(1);
                }
            }
            "--prometheus" => prometheus = true,
            other => {
                eprintln!("ERROR: unknown option {other:?}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

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
    let sb: Superblock = *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
    if sb.magic != LIONFS_MAGIC {
        eprintln!("ERROR: not a LionFS image (bad magic)");
        std::process::exit(1);
    }

    let profile = RaidProfile::from_u8(sb.raid_profile);
    let devs = devices.unwrap_or_else(|| profile.min_devices() as u32);
    if devs < profile.min_devices() as u32 {
        eprintln!(
            "ERROR: {devs} devices is below the {profile:?} minimum ({})",
            profile.min_devices()
        );
        std::process::exit(1);
    }

    let stats = compute_stats(&sb);
    let utilization = if stats.total_blocks > 0 {
        1.0 - (stats.free_blocks as f64 / stats.total_blocks as f64)
    } else {
        0.0
    };
    let assessment = assess_pool(profile, devs, mttf, mttr);
    let wa = write_amplification(utilization);

    if prometheus {
        print!("{}", render_prometheus(&assessment, Some(utilization)));
        return;
    }

    println!("LionFS reliability prediction — {image}");
    println!("  profile:            {:?}", profile);
    println!("  devices:            {devs}");
    println!("  stripe layout:      k={} m={} groups={}", assessment.layout.k, assessment.layout.m, assessment.layout.groups);
    println!("  device MTTF:        {mttf:.0} h");
    println!("  repair MTTR:        {mttr:.0} h");
    println!();
    println!("  MTTDL (corrected):  {}", format_hours(assessment.mttdl_ctmc_hours));
    println!("  MTTDL (spec model): {} — the naive constant, kept for comparison (off by m!(m+1))", format_hours(assessment.mttdl_spec_hours));
    println!("  availability:       {:.12} (pool lower bound)", assessment.pool_availability);
    println!();
    println!("  utilization:        {utilization:.3} ({} of {} blocks used)", stats.total_blocks - stats.free_blocks, stats.total_blocks);
    if let Some((cow, spec)) = wa {
        println!("  write amp (CoW):    {cow:.3}x — the corrected 1/(1-u) model");
        println!("  write amp (spec):   {spec:.3}x — the erroneous model, kept for audit comparison");
    } else {
        println!("  write amp:          n/a (utilization outside (0,1))");
    }
    println!();
    println!("  Re-run with --prometheus for the metrics exposition.");
}
