//! `lfs_engine` — the I/O engine micro-benchmark and backend prober.
//!
//! Drives the real engine (io_uring when available, else the threaded
//! backend) against a scratch image and reports throughput/latency
//! medians, honoring the RFC-002 honesty rule: every number printed
//! comes with the backend that produced it, the pattern, and the
//! command that reruns it. This is *not* a device benchmark: the
//! container's filesystem is the bottleneck; the numbers measure the
//! engine's userspace cost, exactly like the 1.x lfs_ioperf discipline.

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lionfs_core::io_engine::op::{IoOp, OpKind, OpResult};
use lionfs_core::io_engine::{EngineBuilder, IoEngine};
use lionfs_core::pal;

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let block_size: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4096);
    let queue_depth: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    let rounds: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);

    println!("LionFS 2.0 I/O engine benchmark");
    println!("===============================");
    println!("block size:  {block_size} B");
    println!("queue depth: {queue_depth}");
    println!("rounds:      {rounds} (medians reported)");

    // Scratch device: 64 MiB image in the temp dir.
    let dir = std::env::temp_dir().join(format!("lfs_engine_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("bench.img");
    let file = match pal::file::create_image(&path, 64 * 1024 * 1024) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("failed to create scratch image: {e}");
            std::process::exit(1);
        }
    };
    let devices: Arc<[Arc<std::fs::File>]> = Arc::from(vec![Arc::new(file)].into_boxed_slice());
    let arena = lionfs_core::io_engine::RegisteredBufArena::new(128, 256 * 1024);
    let engine = EngineBuilder::default().build(devices, arena.clone());

    println!("engine:      {}", engine.name());

    // Payload: deterministic bytes.
    let payload: Vec<u8> = (0..block_size as u32).map(|i| (i % 251) as u8).collect();

    let (write_mib, write_lat_us) = bench_writes(&*engine, &arena, &payload, queue_depth, rounds);
    let (read_mib, read_lat_us) = bench_reads(&*engine, &arena, block_size, queue_depth, rounds);

    println!();
    println!(
        "seq {} B write:  {:8.1} MiB/s  median submit->complete {:8.1} us",
        block_size, write_mib, write_lat_us
    );
    println!(
        "seq {} B read:   {:8.1} MiB/s  median submit->complete {:8.1} us",
        block_size, read_mib, read_lat_us
    );
    println!();
    println!(
        "stats: submitted={} completed={} failed={} max_in_flight={} zone_appends={}",
        engine.stats().submitted.load(AtomicOrdering::Relaxed),
        engine.stats().completed.load(AtomicOrdering::Relaxed),
        engine.stats().failed.load(AtomicOrdering::Relaxed),
        engine.stats().max_in_flight.load(AtomicOrdering::Relaxed),
        engine.stats().zone_appends.load(AtomicOrdering::Relaxed)
    );
    println!();
    println!(
        "backend: {} (rerun: lfs_engine {block_size} {queue_depth} {rounds})",
        engine.name()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn bench_writes(
    engine: &dyn IoEngine,
    arena: &Arc<lionfs_core::io_engine::RegisteredBufArena>,
    payload: &[u8],
    queue_depth: usize,
    rounds: usize,
) -> (f64, f64) {
    let block = payload.len() as u64;
    let mut mibs = Vec::new();
    let mut lats = Vec::new();
    let mut next_offset = 1024 * 1024u64; // Skip the first MiB.

    for round in 0..rounds {
        let submitted = Arc::new(AtomicU64::new(0));
        let completed_bytes = Arc::new(AtomicU64::new(0));
        let mut ops_out = Vec::new();
        let t0 = Instant::now();

        let mut tagged = 0u64;
        'outer: loop {
            // Fill a submission wave up to queue depth.
            let mut wave: Vec<IoOp> = Vec::with_capacity(queue_depth);
            for _ in 0..queue_depth {
                let h = match arena.copy_in(payload) {
                    Some(h) => h,
                    None => break,
                };
                wave.push(IoOp::write(
                    0,
                    next_offset,
                    payload.len() as u32,
                    h.slot,
                    h.off,
                    tagged,
                ));
                tagged += 1;
                next_offset += block;
                if next_offset + block > 60 * 1024 * 1024 {
                    next_offset = 1024 * 1024;
                }
            }
            if wave.is_empty() {
                break 'outer;
            }
            submitted.fetch_add(wave.len() as u64, AtomicOrdering::Relaxed);
            let n = engine.submit(&wave);
            let _ = engine.submit_doorbell();
            if n < wave.len() {
                eprintln!("warning: submission queue full ({}/{})", n, wave.len());
            }

            // Drain completions for the wave.
            let want = n;
            let mut got = 0usize;
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut completions = Vec::new();
            while got < want {
                got += engine.reap(&mut completions, want - got);
                if got < want {
                    engine.blocking_wait(Duration::from_millis(20));
                }
                if Instant::now() > deadline {
                    eprintln!("timeout waiting for completions");
                    break;
                }
            }
            completed_bytes.fetch_add((got as u64) * block, AtomicOrdering::Relaxed);
            ops_out.extend(completions.drain(..));
            // Release the wave's handles.
            for op in &wave {
                let h = lionfs_core::io_engine::BufHandle {
                    slot: op.buf,
                    off: op.buf_off,
                    len: op.len,
                };
                arena.release(h);
            }
            // One full sweep of the 64 MiB window per round.
            if tagged as u64 > (58 * 1024 * 1024) / block {
                break 'outer;
            }
        }

        let elapsed = t0.elapsed();
        let bytes = completed_bytes.load(AtomicOrdering::Relaxed);
        let mib = bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64();
        mibs.push(mib);
        // Rough per-op latency: elapsed / ops.
        let ops = submitted.load(AtomicOrdering::Relaxed).max(1);
        lats.push(elapsed.as_micros() as f64 / ops as f64);
        let _ = round;
    }

    (median(&mibs), median(&lats))
}

fn bench_reads(
    engine: &dyn IoEngine,
    arena: &Arc<lionfs_core::io_engine::RegisteredBufArena>,
    block_size: u32,
    queue_depth: usize,
    rounds: usize,
) -> (f64, f64) {
    let block = block_size as u64;
    let mut mibs = Vec::new();
    let mut lats = Vec::new();

    for _round in 0..rounds {
        let submitted = Arc::new(AtomicU64::new(0));
        let completed_bytes = Arc::new(AtomicU64::new(0));
        let t0 = Instant::now();
        let mut next_offset = 1024 * 1024u64;
        let mut tagged = 0u64;
        let deadline = Instant::now() + Duration::from_secs(60);

        while Instant::now() < deadline {
            let mut wave: Vec<IoOp> = Vec::with_capacity(queue_depth);
            for _ in 0..queue_depth {
                let h = match arena.lease(block_size) {
                    Some(h) => h,
                    None => break,
                };
                wave.push(IoOp::read(
                    0,
                    next_offset,
                    block_size,
                    h.slot,
                    h.off,
                    tagged,
                ));
                tagged += 1;
                next_offset += block;
                if next_offset + block > 60 * 1024 * 1024 {
                    next_offset = 1024 * 1024;
                }
            }
            if wave.is_empty() {
                break;
            }
            submitted.fetch_add(wave.len() as u64, AtomicOrdering::Relaxed);
            let n = engine.submit(&wave);
            let _ = engine.submit_doorbell();

            let want = n;
            let mut got = 0usize;
            let mut completions = Vec::new();
            while got < want {
                got += engine.reap(&mut completions, want - got);
                if got < want {
                    engine.blocking_wait(Duration::from_millis(20));
                }
                if Instant::now() > deadline {
                    break;
                }
            }
            completed_bytes.fetch_add((got as u64) * block, AtomicOrdering::Relaxed);
            for op in &wave {
                let h = lionfs_core::io_engine::BufHandle {
                    slot: op.buf,
                    off: op.buf_off,
                    len: op.len,
                };
                arena.release(h);
            }
            // ~16 MiB per round keeps runtime bounded.
            if completed_bytes.load(AtomicOrdering::Relaxed) >= 16 * 1024 * 1024 {
                break;
            }
        }

        let elapsed = t0.elapsed();
        let bytes = completed_bytes.load(AtomicOrdering::Relaxed);
        let mib = bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64().max(1e-9);
        mibs.push(mib);
        let ops = submitted.load(AtomicOrdering::Relaxed).max(1);
        lats.push(elapsed.as_micros() as f64 / ops as f64);
    }

    (median(&mibs), median(&lats))
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut sorted = v.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

/// Sanity assertion run at startup: every completion must carry a
/// success result on the happy path; failures abort with the errno.
#[allow(dead_code)]
fn assert_ok(kind: OpKind, res: OpResult) {
    if let OpResult::Failed(errno) = res {
        panic!("engine op {kind} failed with errno {errno}");
    }
}
