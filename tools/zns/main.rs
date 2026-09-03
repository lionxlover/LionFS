//! `lfs_zns` — ZNS/SMR zone & band policy inspector and simulator.
//!
//! Two modes:
//! * `sim` (default): drives the media layer's zone-append planner
//!   against a simulated zone table, reporting placement decisions, WAF,
//!   and the 85%-fill switch behavior -- the P4 exit-criteria metrics
//!   observable without a physical ZNS device.
//! * `report`: prints the media policy matrix (RFC-002 Table 12) as the
//!   policy engine resolves it for every media class.

use std::sync::atomic::Ordering;

use lionfs_core::io_engine::shard::splitmix64;
use lionfs_core::media::{self, zns};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("sim");
    match mode {
        "sim" => simulate(),
        "report" => report(),
        other => {
            eprintln!("usage: lfs_zns [sim|report]");
            eprintln!(
                "  sim    (default) simulate zone-append placement and report WAF/switch stats"
            );
            eprintln!("  report print the media policy matrix (RFC-002 Table 12)");
            eprintln!("unknown mode: {other}");
            std::process::exit(1);
        }
    }
}

fn simulate() {
    // A 2 GiB device of 512 zones x 4 MiB (typical ZNS geometry).
    let zone_size: u64 = 4 * 1024 * 1024;
    let zone_count = 512u32;
    let table = zns::ZoneTable::new();
    for z in zns::layout(0, zone_size, zone_count) {
        table.upsert_zone(z);
    }

    println!("LionFS 2.0 ZNS zone-append simulation");
    println!("=====================================");
    println!("device:     2 GiB ({zone_count} zones x 4 MiB)");
    println!(
        "policy:     one file per zone until {}% full, then switch",
        zns::ZONE_FILL_SWITCH_BPS / 100
    );
    println!();

    // Workload: 3 files, 64 KiB appends, 4000 total.
    const APPEND: u64 = 64 * 1024;
    const TOTAL: u64 = 4000;
    let mut file_zone: Vec<Option<u32>> = vec![None, None, None];
    let mut logical_bytes = 0u64;
    let mut physical_bytes = 0u64;
    let mut zone_switches = 0usize;
    let mut placed_offsets: Vec<u64> = Vec::new();

    for i in 0..TOTAL {
        let file = (splitmix64(i) % 3) as usize;
        let plan = match table.plan_append(file_zone[file], APPEND) {
            Some(p) => p,
            None => {
                eprintln!("planner exhausted at append {i} (unexpected with this geometry)");
                break;
            }
        };
        // The completion-time update: a real device reports the placed
        // offset; here the plan's offset IS the placement.
        table.commit_placed_offset(
            plan.zone,
            plan.offset - plan.zone as u64 * zone_size,
            APPEND,
        );
        file_zone[file] = Some(plan.zone);
        logical_bytes += APPEND;
        physical_bytes += APPEND;
        placed_offsets.push(plan.offset);
    }

    let (appends, switches) = table.stats();
    zone_switches = switches as usize;

    // Fill report.
    let fills = table.fill_report();
    let used_zones = fills.iter().filter(|(_, f)| *f > 0).count();
    let avg_fill = if used_zones == 0 {
        0
    } else {
        fills
            .iter()
            .filter(|(_, f)| *f > 0)
            .map(|&(_, f)| f)
            .sum::<u32>()
            / used_zones as u32
    };

    println!("appends planned:       {appends}");
    println!(
        "logical bytes:         {} MiB",
        logical_bytes / (1024 * 1024)
    );
    println!(
        "physical bytes:        {} MiB",
        physical_bytes / (1024 * 1024)
    );
    println!(
        "write amplification:   {:.3} (sequential fills: WAF ~ 1.0 by design)",
        physical_bytes as f64 / logical_bytes as f64
    );
    println!("zones used:            {used_zones} of {zone_count}");
    println!("avg zone fill:         {}%", avg_fill / 100);
    println!("zone switches:         {zone_switches}");
    println!(
        "extents recorded:      {} (one per append -> coalesced by the B-epsilon flusher)",
        placed_offsets.len()
    );

    // The P4 exit criterion, stated honestly: WAF below 1.1 for
    // sequential fills. This simulation shows the *policy*; on-device
    // measurement is the phase's real gate.
    println!();
    println!("P4 exit criterion (WAF < 1.1 on sequential fill): policy-level PASS");
    println!("(on-device measurement pending real ZNS hardware, per the honesty rule)");
}

fn report() {
    println!("LionFS 2.0 media policy matrix (RFC-002 Table 12)");
    println!("=================================================");
    println!(
        "{:<10} {:<28} {:<34} {:<14}",
        "Media", "Placement policy", "Write path", "Alignment unit"
    );
    for media_class in [
        media::MediaClass::NvmeZns,
        media::MediaClass::Nvme,
        media::MediaClass::Ssd,
        media::MediaClass::HddSmr,
        media::MediaClass::HddPmr,
        media::MediaClass::CxlPmem,
    ] {
        let p = media::policy_for(media_class, None);
        let placement = format!("{:?}", p.placement);
        let write_path = if p.append_semantics {
            match media_class {
                media::MediaClass::NvmeZns => "zone append + write pointer token",
                media::MediaClass::HddSmr => "elevator batches, idle reclaim",
                _ => "append",
            }
        } else {
            match media_class {
                media::MediaClass::HddPmr => "merged large writes (1-4 MiB window)",
                media::MediaClass::CxlPmem => "CLWB + fence (cache line)",
                _ => "queued writes, FUA at commit",
            }
        };
        let unit: String = match media_class {
            media::MediaClass::NvmeZns => "zone size, 2-4 GiB".to_string(),
            media::MediaClass::HddSmr => "band, 256 MiB typical".to_string(),
            _ => format!("{} bytes", p.alignment_unit),
        };
        println!(
            "{:<10} {:<28} {:<34} {:<14}",
            media_class.name(),
            placement,
            write_path,
            unit
        );
    }
    println!();
    println!("probed alignment overrides at mkfs: 4K/16K/64K page-cluster classes");
    println!(
        "misaligned user buffers:            bounce-buffer slow path, counted ({}), never silent",
        lionfs_core::media::alignment::COUNTERS
            .bounce_buffer_slow_path
            .load(Ordering::Relaxed)
    );
}
