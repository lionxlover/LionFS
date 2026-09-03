//! `lfs_gc` -- copy-GC planner inspection (RFC-004 §6).
//!
//! `lfs_gc sim` runs a synthetic pool through the planner across the
//! watermark bands (healthy -> background -> aggressive) and prints
//! what it would do: the urgency verdict, selected segments, and the
//! copy/reclaim accounting -- the planner's decision loop, observable
//! without a pool.

use lionfs_core::gc::{GcConfig, GcPlanner, GcUrgency, SegmentStat};

const SEG: u64 = 256 << 20; // 256 MiB segments
const HOUR: u64 = 3_600_000_000_000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("sim") | None => sim(),
        Some(other) => {
            eprintln!("usage: lfs_gc [sim]");
            let _ = other;
            std::process::exit(1);
        }
    }
}

fn sim() {
    println!("LionFS copy-GC planner simulation (RFC-004 §6)");
    let planner = GcPlanner::default();
    let cfg: &GcConfig = planner.config();
    println!(
        "watermarks: kick {}%, aggressive {}%, max {}/plan, age half-life {}d",
        cfg.kick_pct,
        cfg.aggressive_pct,
        cfg.max_segments_per_plan,
        cfg.age_half_life_ns / (24 * HOUR)
    );
    println!();

    // A pool of 64 segments, 256 MiB each (16 GiB total).
    let mut segments = Vec::new();
    for i in 0..64u64 {
        // A realistic mix: mostly-cold, mostly-live segments with a few
        // churned ones.
        let live = match i % 4 {
            0 => SEG / 10, // 90% dead, old
            1 => SEG / 2,  // 50% dead, old
            2 => 9 * SEG / 10, // 10% dead, old
            _ => SEG / 4,  // 75% dead, young (hot churn)
        };
        let age = if i % 4 == 3 { HOUR } else { 30 * 24 * HOUR };
        segments.push(SegmentStat {
            segment_id: i,
            total_bytes: SEG,
            live_bytes: live,
            age_ns: age,
            write_cycles: i * 37,
        });
    }

    for free_pct in [50, 15, 5, 2] {
        let total = 64 * SEG;
        let free = total * free_pct / 100;
        println!("-- pool free: {}% ({} MiB)", free_pct, free >> 20);
        match planner.plan(&segments, total, free) {
            None => {
                let label = if u64::from(free_pct) >= u64::from(cfg.kick_pct) { "healthy: no plan" } else { "all-live pool: nothing reclaimable" };
                println!("   plan:      {label}");
            }
            Some(plan) => {
                let urgency = match plan.urgency {
                    GcUrgency::Idle => "idle",
                    GcUrgency::Background => "background",
                    GcUrgency::Aggressive => "AGGRESSIVE (panic mode)",
                };
                println!("   urgency:   {urgency}");
                println!(
                    "   segments:  {:?} ({} of {} candidates)",
                    plan.segments,
                    plan.segments.len(),
                    segments.iter().filter(|s| s.freeable_bytes() > 0).count()
                );
                println!(
                    "   reclaim:   {} MiB for {} MiB of copy IO ({:.2}x efficiency)",
                    plan.estimated_reclaimed_bytes >> 20,
                    plan.estimated_copy_bytes >> 20,
                    plan.estimated_reclaimed_bytes as f64 / plan.estimated_copy_bytes.max(1) as f64
                );
            }
        }
        println!();
    }

    // Wear demonstration: identical segments except write cycles.
    let fresh = SegmentStat { segment_id: 100, total_bytes: SEG, live_bytes: SEG / 2, age_ns: 24 * HOUR, write_cycles: 10 };
    let worn = SegmentStat { segment_id: 101, total_bytes: SEG, live_bytes: SEG / 2, age_ns: 24 * HOUR, write_cycles: 50_000 };
    println!(
        "wear leveling: fresh segment scores {} vs worn {} (worn loses)",
        planner.score(&fresh, false),
        planner.score(&worn, false)
    );
}
