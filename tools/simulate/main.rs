//! `lfs_simulate` -- the deterministic crash simulator's front door
//! (Phase 8, ②).
//!
//! Three modes:
//!
//! * `lfs_simulate run [--seed S] [--ops N] [--crash-at K]` --
//!   one scripted universe, optionally crashed at op K, printing the
//!   run report and the invariant verdicts.
//! * `lfs_simulate sweep [--seed S] [--ops N]` -- the exhaustive
//!   crash-point sweep: one universe per crash op, every invariant
//!   checked at every point (the FoundationDB discipline).
//! * `lfs_simulate determinism [--seed S]` -- runs the same seed
//!   twice and proves the reports are bit-identical.
//!
//! Every number printed is a function of (seed, ops, crash point)
//! and nothing else -- no clocks, no addresses, no platform state.
//! A field report of "crash at op #173 with seed 9927 misbehaved"
//! reproduces exactly here.

use lionfs_core::sim::{CrashMode, CrashSimulator};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut seed: u64 = 42;
    let mut ops: usize = 120;
    let mut crash_at: Option<usize> = None;
    let mut mode = "run";

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" | "-s" => {
                i += 1;
                seed = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(seed);
            }
            "--ops" | "-n" => {
                i += 1;
                ops = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(ops);
            }
            "--crash-at" | "-k" => {
                i += 1;
                crash_at = args.get(i).and_then(|v| v.parse().ok());
            }
            "run" | "sweep" | "determinism" => mode = &args[i],
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("usage: lfs_simulate [run|sweep|determinism] [--seed S] [--ops N] [--crash-at K]");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    match mode {
        "run" => run(seed, ops, crash_at),
        "sweep" => sweep(seed, ops),
        "determinism" => determinism(seed, ops),
        _ => unreachable!(),
    }
}

fn run(seed: u64, ops: usize, crash_at: Option<usize>) {
    let mode = match crash_at {
        Some(at) => CrashMode::AfterOp { at },
        None => CrashMode::None,
    };
    println!("LionFS deterministic crash simulator -- single run");
    println!("seed: {seed}, ops: {ops}, mode: {}", match mode {
        CrashMode::None => "clean (no crash)".to_owned(),
        CrashMode::AfterOp { at } => format!("power cut after op {at}"),
    });
    let r = CrashSimulator::new(seed).run(ops, mode);
    print_report(&r);
    println!();
    if r.crashed {
        println!("invariants: prefix={}, overlay-converged={}",
            if r.prefix_property_held { "HELD" } else { "VIOLATED" },
            if r.overlay_converged { "HELD" } else { "VIOLATED" },
        );
        assert!(r.prefix_property_held && r.overlay_converged);
    } else {
        println!("invariants: prefix={} (clean run, trivially held)",
            if r.prefix_property_held { "HELD" } else { "VIOLATED" });
    }
}

fn sweep(seed: u64, ops: usize) {
    println!("LionFS deterministic crash simulator -- exhaustive crash-point sweep");
    println!("seed: {seed}, script length: {ops} ops -> {ops} crash points");
    let reports = CrashSimulator::sweep(seed, ops);
    let mut torn = 0usize;
    let mut replayed_min = u64::MAX;
    let mut replayed_max = 0u64;
    for (at, r) in reports.iter().enumerate() {
        assert!(r.prefix_property_held, "prefix property failed at op {at}");
        assert!(r.overlay_converged, "overlay convergence failed at op {at}");
        assert!(r.replayed <= r.ledger_entries, "replay exceeded ledger at op {at}");
        if r.replay_tail.is_some() {
            torn += 1;
        }
        replayed_min = replayed_min.min(r.replayed);
        replayed_max = replayed_max.max(r.replayed);
    }
    println!("crash points swept : {}", reports.len());
    println!("torn tails         : {torn} (discarded cleanly, by construction)");
    println!("replayed records   : min {replayed_min}, max {replayed_max}");
    println!("invariants         : ALL HELD at every crash point");
}

fn determinism(seed: u64, ops: usize) {
    println!("LionFS deterministic crash simulator -- determinism proof");
    let at = ops / 3;
    let a = CrashSimulator::new(seed).run(ops, CrashMode::AfterOp { at });
    let b = CrashSimulator::new(seed).run(ops, CrashMode::AfterOp { at });
    let identical = a == b;
    println!("seed {seed}, ops {ops}, crash at {at}: run A == run B -> {identical}");
    print_report(&a);
    assert!(identical, "same seed must produce the same universe");
    println!("determinism: PROVEN (bit-identical reports)");
}

fn print_report(r: &lionfs_core::sim::SimReport) {
    println!("  ops executed     : {}", r.ops);
    println!("  log writes       : {} (record-log path)", r.log_writes);
    println!("  tree writes      : {} (B-epsilon path)", r.tree_writes);
    println!("  window commits   : {}", r.commits);
    println!("  checkpoint drains: {}", r.checkpoints);
    println!("  qos delays       : {}", r.qos_delays);
    println!("  gc rounds        : {}", r.gc_rounds);
    println!("  retention passes : {} (expired {})", r.retention_passes, r.retention_expired);
    if r.crashed {
        println!("  crashed          : yes");
        println!("  ledger records   : {}", r.ledger_entries);
        println!("  replayed records : {}", r.replayed);
        println!("  replay tail      : {}", match r.replay_tail {
            None => "clean end of log".to_owned(),
            Some(t) => format!("{t:?} (discarded)"),
        });
    } else {
        println!("  crashed          : no (clean control run)");
    }
}
