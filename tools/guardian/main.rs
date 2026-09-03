//! `lfs_guardian` -- the Guardian agent runner (RFC-004 §7).
//!
//! `lfs_guardian sim` feeds a synthetic telemetry window through the
//! full agent pipeline (entropy watch, drive-risk assessment,
//! workload classification) and prints the advisory bus -- the
//! out-of-band AI operations loop, observable end-to-end.
//!
//! `lfs_guardian watch <mount>` is the production posture: it reads
//! the daemon's telemetry socket and emits advisories; wiring is the
//! Phase-8 integration (ROADMAP).

use lionfs_core::guardian::{
    entropy, Agent, AgentConfig, DriveTelemetry, EntropyWatcher, FailurePredictor,
    WorkloadClassifier,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("sim");
    match mode {
        "sim" | "simulate" => simulate(),
        "watch" => {
            let mount = args.get(2).map(String::as_str).unwrap_or("/mnt/lion");
            eprintln!(
                "watch mode: attach to {mount}/.lfs_telemetry (Phase-8 wiring; use `sim` for the offline pipeline)"
            );
            std::process::exit(2);
        }
        _ => {
            eprintln!("usage: lfs_guardian [sim|watch <mount>]");
            std::process::exit(1);
        }
    }
}

fn simulate() {
    println!("LionFS Guardian -- offline pipeline simulation (RFC-004 §7)");
    println!();

    let mut agent = Agent::new(AgentConfig::default());
    let mut watcher = EntropyWatcher::new();
    let mut classifier = WorkloadClassifier::new();
    let predictor = FailurePredictor::default();

    // --- Phase 1: a quiet text-ish workload. --------------------------
    println!("-- window 1..8: quiet workload (text writes, new files)");
    let text = b"the quick brown fox jumps over the lazy dog. ".repeat(24);
    for _ in 1..=8 {
        agent.tick();
        watcher.observe(100, 5, 0, &text);
        classifier.observe(64 * 1024, false, 8192);
    }
    println!(
        "   entropy EWMA: {:.2} bits/byte, suspicion {} bps (freeze at {})",
        (watcher.evidence().0 >> 32) as f64,
        watcher.suspicion().score_bps,
        entropy::FREEZE_BPS
    );
    println!("   workload class: {}", classifier.classify().tag());

    // --- Phase 2: the ransomware signature appears. --------------------
    println!("-- window 9..24: rewrite-encrypt-everything signature");
    let ciphertext: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(0x9E37_79B1) >> 24) as u8)
        .collect();
    for _ in 9..=24 {
        agent.tick();
        watcher.observe(100, 100, 100, &ciphertext);
        agent.observe_suspicion(watcher.suspicion());
    }
    let advisories = agent.drain();
    for a in &advisories {
        println!(
            "   ADVISORY [{}] action={:?} evidence={} window={}",
            a.kind.name(),
            a.action,
            a.evidence,
            a.window
        );
    }
    println!(
        "   entropy now: {:.2} bits/byte, suspicion {} bps -> freeze_recommended={}",
        (watcher.evidence().0 >> 32) as f64,
        watcher.suspicion().score_bps,
        watcher.suspicion().freeze_recommended
    );

    // --- Phase 3: a degrading drive shows up. ---------------------------
    println!("-- drive telemetry: realloc burst on device 0");
    let telemetry = DriveTelemetry {
        realloc_events: 5,
        pending_sectors: 3,
        crc_errors: 2,
        median_latency_us: 500,
        p99_latency_us: 1_400,
        power_on_hours: 40_000,
        scrub_repairs: 1,
    };
    let assessment = predictor.assess(&telemetry);
    agent.tick();
    agent.observe_drive(0, &assessment);
    for a in agent.drain() {
        println!(
            "   ADVISORY [{}] action={:?} band={} multiplier={}x remaining~{}h",
            a.kind.name(),
            a.action,
            assessment.band.name(),
            assessment.hazard_multiplier_x100 / 100,
            assessment.est_remaining_hours
        );
    }

    // --- Phase 4: the workload shifts to a DB profile. ------------------
    println!("-- workload shift: 8K random RW with 10% syncs (a database moved in)");
    for _ in 0..20 {
        agent.tick();
        classifier.observe_window(&lionfs_core::guardian::workload::WindowStats {
            ops: 10_000,
            bytes: 80 << 20,
            reads: 5_000,
            syncs: 1_000,
            max_seq_run_bytes: 8 << 20,
        });
    }
    let class = classifier.classify();
    agent.tick();
    agent.observe_workload(class);
    for a in agent.drain() {
        println!(
            "   ADVISORY [{}] action={:?} class={}",
            a.kind.name(),
            a.action,
            class.tag()
        );
    }

    println!();
    println!("pipeline complete; 0 actions touched the data path.");
}
