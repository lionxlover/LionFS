//! Engine backends: the [`IoEngine`] trait, the portable
//! [`ThreadedEngine`], and runtime selection via [`EngineBuilder`].
//!
//! The trait is the whole contract the transaction layer sees. It is
//! intentionally *pull*-shaped on completions (reap) rather than
//! callback-shaped: the RFC's completion path (§9.1 steps 8-9) batches CQE
//! reaping, and a callback design would put transaction code inside the
//! engine's reaper thread -- the exact kind of coupling that makes lock
//! ordering unprovable.
//!
//! The [`ThreadedEngine`] is not a toy: it is the correctness floor and
//! the CI workhorse. Each worker owns a device-file handle reference and
//! executes positioned I/O through the PAL; there are no locks on the
//! submission path (hand-off is the MPMC queue) and no allocation after
//! warm-up (ops and completions are pre-shaped POD). What it cannot
//! remove -- one MPMC hop per op, blocking device waits per worker --
//! is exactly what the io_uring backend exists to remove, and the
//! difference is measured by `tools/lfs_engine` rather than asserted.

use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::mpmc::MpmcQueue;
use super::op::{Completion, IoOp, OpKind, OpResult};
use super::zero_copy::RegisteredBufArena;
use crate::pal;

/// Live engine statistics, all monotonic counters (health-bus shape).
#[derive(Debug, Default)]
pub struct EngineStats {
    pub submitted: AtomicU64,
    pub completed: AtomicU64,
    pub failed: AtomicU64,
    pub flushes: AtomicU64,
    pub zone_appends: AtomicU64,
    pub max_in_flight: AtomicU64,
}

impl EngineStats {
    pub(crate) fn note_in_flight(&self, current: u64) {
        let mut cur = self.max_in_flight.load(Ordering::Relaxed);
        while current > cur {
            match self.max_in_flight.compare_exchange_weak(
                cur,
                current,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }
}

/// A running device-submission engine.
///
/// Concurrency contract: `submit` may be called from any shard thread;
/// `reap` is called by the single completion dispatcher; `blocking_wait`
/// parks that dispatcher until completions are likely available (or the
/// timeout expires). Backend implementors must keep `submit` lock-free
/// and allocation-free in steady state.
pub trait IoEngine: Send + Sync {
    /// Backend name ("io_uring", "threaded").
    fn name(&self) -> &'static str;
    /// Enqueues ops; returns how many actually entered the queue (a short
    /// count means the submission queue filled: backpressure, batch the
    /// remainder).
    fn submit(&self, ops: &[IoOp]) -> usize;
    /// Doorbell for backends that need one after a from-empty submission
    /// (io_uring without SQPOLL). No-op elsewhere.
    fn submit_doorbell(&self) -> std::io::Result<()>;
    /// Drains up to `max` completions into `out`; returns the count.
    fn reap(&self, out: &mut Vec<Completion>, max: usize) -> usize;
    /// Parks up to `timeout` for new completions; `true` => reap now.
    fn blocking_wait(&self, timeout: Duration) -> bool;
    /// Access to live counters.
    fn stats(&self) -> &EngineStats;
}

/// Configuration for engine construction, RFC-002 Table 6 defaults.
#[derive(Debug, Clone)]
pub struct EngineBuilder {
    pub sq_depth: usize,
    pub sqpoll: bool,
    pub iopoll: bool,
    pub workers: usize,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self {
            // 1024 entries covers QD 64 x 16 cores without overflow.
            sq_depth: 1024,
            sqpoll: false,
            iopoll: false,
            workers: pal::platform::cpu_count().get(),
        }
    }
}

impl EngineBuilder {
    #[must_use]
    pub fn with_sqpoll(mut self, on: bool) -> Self {
        self.sqpoll = on;
        self
    }

    #[must_use]
    pub fn with_iopoll(mut self, on: bool) -> Self {
        self.iopoll = on;
        self
    }

    /// Builds the best engine available for this host: io_uring when
    /// compiled in and the kernel accepts the ring, else the threaded
    /// engine. Never fails for a missing fast path -- only for genuinely
    /// broken inputs (no devices, empty arena).
    pub fn build(
        &self,
        devices: Arc<[Arc<File>]>,
        arena: Arc<RegisteredBufArena>,
    ) -> Box<dyn IoEngine> {
        #[cfg(all(target_os = "linux", feature = "io_uring"))]
        {
            match super::uring::UringEngine::new(devices.clone(), arena.clone(), self) {
                Ok(engine) => {
                    log::info!(
                        "io_engine: io_uring backend active (sq_depth={}, sqpoll={}, iopoll={})",
                        self.sq_depth,
                        self.sqpoll,
                        self.iopoll
                    );
                    return Box::new(engine);
                }
                Err(e) => {
                    log::warn!(
                        "io_engine: io_uring unavailable ({e}); falling back to threaded backend"
                    );
                }
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
        {
            log::info!("io_engine: threaded backend (io_uring not compiled in)");
        }
        Box::new(ThreadedEngine::new(
            devices,
            arena,
            self.workers,
            self.sq_depth,
        ))
    }
}

// -- threaded backend -----------------------------------------------------------

/// Worker-thread engine: the portable submission plane.
pub struct ThreadedEngine {
    inbox: Arc<MpmcQueue<IoOp>>,
    outbox: Arc<MpmcQueue<Completion>>,
    waker: Arc<pal::waker::Waker>,
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Devices/arena are cloned into the workers; kept here for teardown
    /// diagnostics and future topology introspection.
    devices: Arc<[Arc<File>]>,
    arena: Arc<RegisteredBufArena>,
    stats: Arc<EngineStats>,
    in_flight: Arc<AtomicU64>,
    shutdown: Arc<AtomicU64>,
}

impl ThreadedEngine {
    /// Creates the engine and starts `workers` threads.
    ///
    /// # Panics
    /// If zero devices are supplied, or a worker thread cannot be spawned.
    pub fn new(
        devices: Arc<[Arc<File>]>,
        arena: Arc<RegisteredBufArena>,
        workers: usize,
        queue_depth: usize,
    ) -> Self {
        assert!(!devices.is_empty(), "engine needs at least one device");
        let workers = workers.max(1);
        let depth = queue_depth.max(64);
        let inbox = Arc::new(MpmcQueue::with_capacity_hint(depth));
        let outbox = Arc::new(MpmcQueue::with_capacity_hint(depth * 4));
        let waker = Arc::new(pal::waker::Waker::new().expect("waker creation"));
        let stats = Arc::new(EngineStats::default());
        let in_flight = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let inbox = Arc::clone(&inbox);
            let outbox = Arc::clone(&outbox);
            let devices = Arc::clone(&devices);
            let arena = Arc::clone(&arena);
            let stats = Arc::clone(&stats);
            let in_flight = Arc::clone(&in_flight);
            let shutdown = Arc::clone(&shutdown);
            let waker = Arc::clone(&waker);
            handles.push(
                std::thread::Builder::new()
                    .name("lfs-io-worker".to_string())
                    .spawn(move || {
                        worker_loop(
                            &inbox, &outbox, &devices, &arena, &stats, &in_flight, &shutdown,
                            &waker,
                        );
                    })
                    .expect("engine worker thread spawn"),
            );
        }

        Self {
            inbox,
            outbox,
            waker,
            workers: handles,
            #[allow(dead_code)]
            devices,
            #[allow(dead_code)]
            arena,
            stats,
            in_flight,
            shutdown,
        }
    }
}

impl IoEngine for ThreadedEngine {
    fn name(&self) -> &'static str {
        "threaded"
    }

    fn submit(&self, ops: &[IoOp]) -> usize {
        let mut n = 0;
        for op in ops {
            if self.inbox.push(op.clone()) {
                n += 1;
                self.stats.submitted.fetch_add(1, Ordering::Relaxed);
            } else {
                break;
            }
        }
        let cur = self
            .in_flight
            .fetch_add(n as u64, Ordering::AcqRel)
            .wrapping_add(n as u64);
        self.stats.note_in_flight(cur);
        n
    }

    fn submit_doorbell(&self) -> std::io::Result<()> {
        // Threaded workers poll their inbox; the waker exists to unblock
        // the completion dispatcher, not to start work.
        Ok(())
    }

    fn reap(&self, out: &mut Vec<Completion>, max: usize) -> usize {
        let mut n = 0;
        while n < max {
            match self.outbox.pop() {
                Some(c) => {
                    out.push(c);
                    n += 1;
                }
                None => break,
            }
        }
        if n > 0 {
            self.stats.completed.fetch_add(n as u64, Ordering::Relaxed);
            self.in_flight.fetch_sub(n as u64, Ordering::AcqRel);
        }
        n
    }

    fn blocking_wait(&self, timeout: Duration) -> bool {
        if !self.outbox.is_empty() {
            return true;
        }
        self.waker.wait(timeout) || !self.outbox.is_empty()
    }

    fn stats(&self) -> &EngineStats {
        &self.stats
    }
}

impl Drop for ThreadedEngine {
    fn drop(&mut self) {
        self.shutdown.store(1, Ordering::Release);
        // Workers exit on the shutdown flag; join them so tests do not
        // leak threads.
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn worker_loop(
    inbox: &MpmcQueue<IoOp>,
    outbox: &MpmcQueue<Completion>,
    devices: &Arc<[Arc<File>]>,
    arena: &Arc<RegisteredBufArena>,
    stats: &EngineStats,
    in_flight: &Arc<AtomicU64>,
    shutdown: &Arc<AtomicU64>,
    waker: &Arc<pal::waker::Waker>,
) {
    loop {
        if shutdown.load(Ordering::Acquire) != 0 {
            return;
        }
        match inbox.pop() {
            Some(op) => {
                let completion = execute_op(&op, devices, arena);
                match completion.result {
                    OpResult::Failed(e) => {
                        stats.failed.fetch_add(1, Ordering::Relaxed);
                        log::debug!("io_engine: op {:?} failed: errno {e}", op.kind);
                    }
                    OpResult::Done(_) | OpResult::Flushed => {
                        if op.kind == OpKind::ZoneAppend {
                            stats.zone_appends.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                while !outbox.push(completion) {
                    // Outbox full: the dispatcher is slow. Yield rather
                    // than spin hot; this is a bounded, observable
                    // condition (outbox is 4x the inbox depth).
                    std::thread::yield_now();
                }
                waker.wake();
                let _ = in_flight;
            }
            None => {
                // Idle: sleep in small slices so shutdown stays snappy.
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn execute_op(
    op: &IoOp,
    devices: &Arc<[Arc<File>]>,
    arena: &Arc<RegisteredBufArena>,
) -> Completion {
    let dev = op.device as usize;
    let Some(file) = devices.get(dev) else {
        return Completion::err(op.user_data, op.kind, crate::pal::posix::EINVAL);
    };
    let handle = super::zero_copy::BufHandle {
        slot: op.buf,
        off: op.buf_off,
        len: op.len,
    };
    match op.kind {
        OpKind::Read => {
            // SAFETY: the caller leased this slot exclusively for this op.
            let dst = unsafe { arena.slice_mut(handle) };
            match pal::file::pread_full(file, dst, op.offset) {
                Ok(()) => Completion::data(op.user_data, op.kind, op.len),
                Err(e) => Completion::err(op.user_data, op.kind, pal::posix::io_error_to_errno(&e)),
            }
        }
        OpKind::Write | OpKind::WriteFua => {
            // SAFETY: lease exclusivity as above.
            let src = unsafe { arena.slice(handle) };
            match pal::file::pwrite_full(file, src, op.offset) {
                Ok(n) => {
                    if op.kind == OpKind::WriteFua {
                        // FUA semantics: data must be durable at
                        // completion. The threaded backend has no FUA
                        // pass-through on plain files, so it enforces the
                        // contract with an explicit data barrier -- the
                        // honest portable interpretation.
                        if let Err(e) = pal::sync::sync_data(file) {
                            return Completion::err(
                                op.user_data,
                                op.kind,
                                pal::posix::io_error_to_errno(&e),
                            );
                        }
                    }
                    Completion::data(op.user_data, op.kind, n as u32)
                }
                Err(e) => Completion::err(op.user_data, op.kind, pal::posix::io_error_to_errno(&e)),
            }
        }
        OpKind::FlushData => match pal::sync::sync_data(file) {
            Ok(()) => Completion::ok(op.user_data, op.kind),
            Err(e) => Completion::err(op.user_data, op.kind, pal::posix::io_error_to_errno(&e)),
        },
        OpKind::ZoneAppend => {
            // Portable simulation of zone-append: the caller resolved the
            // zone's write pointer into op.offset (see media::zns --
            // ZoneTable::plan_append); we write there and echo it as the
            // placed offset, which is byte-for-byte the completion shape
            // the real IORING_OP_ZONE_APPEND produces.
            // SAFETY: lease exclusivity.
            let src = unsafe { arena.slice(handle) };
            match pal::file::pwrite_full(file, src, op.offset) {
                Ok(n) => Completion::zone_append(op.user_data, op.offset, n as u32),
                Err(e) => Completion::err(op.user_data, op.kind, pal::posix::io_error_to_errno(&e)),
            }
        }
        OpKind::Deallocate => {
            // No portable punch-hole primitive via positioned I/O; the
            // allocator treats deallocates logically. Report success with
            // zero bytes.
            Completion::data(op.user_data, op.kind, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_device(tag: &str, size: u64) -> Arc<File> {
        let dir = std::env::temp_dir().join(format!("lionfs_engine_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}.img"));
        Arc::new(pal::file::create_image(&path, size).unwrap())
    }

    fn test_engine() -> (Box<dyn IoEngine>, Arc<RegisteredBufArena>) {
        let dev = temp_device("basic", 8 * 1024 * 1024);
        let devices: Arc<[Arc<File>]> = Arc::from(vec![dev].into_boxed_slice());
        let arena = RegisteredBufArena::new(32, 64 * 1024);
        let engine = EngineBuilder::default().build(devices, arena.clone());
        (engine, arena)
    }

    fn wait_for_completions(engine: &dyn IoEngine, want: usize, out: &mut Vec<Completion>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while out.len() < want {
            if std::time::Instant::now() > deadline {
                panic!(
                    "timed out waiting for {want} completions, got {}",
                    out.len()
                );
            }
            engine.reap(out, want - out.len());
            if out.len() < want {
                engine.blocking_wait(Duration::from_millis(50));
            }
        }
    }

    #[test]
    fn write_then_read_roundtrip() {
        let (engine, arena) = test_engine();
        let h = arena.copy_in(&[0xA5u8; 4096]).expect("arena");
        let ops = [IoOp::write(0, 8192, 4096, h.slot, h.off, 77)];
        assert_eq!(engine.submit(&ops), 1);
        let mut out = Vec::new();
        wait_for_completions(engine.as_ref(), 1, &mut out);
        assert_eq!(out[0].user_data, 77);
        assert_eq!(out[0].result, OpResult::Done(4096));

        let h2 = arena.lease(4096).expect("arena");
        let ops = [IoOp::read(0, 8192, 4096, h2.slot, h2.off, 78)];
        assert_eq!(engine.submit(&ops), 1);
        let mut out = Vec::new();
        wait_for_completions(engine.as_ref(), 1, &mut out);
        assert_eq!(out[0].user_data, 78);
        // SAFETY: leased by this test thread, op completed.
        let data = unsafe { arena.slice(h2) };
        assert!(data.iter().all(|&b| b == 0xA5));
    }

    #[test]
    fn zone_append_places_at_hinted_pointer() {
        let (engine, arena) = test_engine();
        let h = arena.copy_in(&[0x11u8; 512]).unwrap();
        // The "zone write pointer" for this simulated zone is 0x2_0000.
        let mut op = IoOp::zone_append(0, 3, 512, h.slot, h.off, 90);
        op.offset = 0x2_0000;
        assert_eq!(engine.submit(&[op]), 1);
        let mut out = Vec::new();
        wait_for_completions(engine.as_ref(), 1, &mut out);
        assert_eq!(out[0].placed_offset, 0x2_0000);
        assert_eq!(out[0].result, OpResult::Done(512));
        assert!(engine.stats().zone_appends.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn flush_data_completes() {
        let (engine, arena) = test_engine();
        let ops = [IoOp::flush_data(0, 1)];
        assert_eq!(engine.submit(&ops), 1);
        let mut out = Vec::new();
        wait_for_completions(engine.as_ref(), 1, &mut out);
        assert_eq!(out[0].result, OpResult::Flushed);
    }

    #[test]
    fn short_read_past_eof_is_failure_not_garbage() {
        let (engine, arena) = test_engine();
        let h = arena.lease(4096).unwrap();
        // Device is 8 MiB; read at EOF.
        let ops = [IoOp::read(0, 8 * 1024 * 1024, 4096, h.slot, h.off, 3)];
        assert_eq!(engine.submit(&ops), 1);
        let mut out = Vec::new();
        wait_for_completions(engine.as_ref(), 1, &mut out);
        assert!(out[0].result.is_err());
        assert!(engine.stats().failed.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn invalid_device_index_fails_cleanly() {
        let (engine, arena) = test_engine();
        let h = arena.lease(16).unwrap();
        let ops = [IoOp::read(9, 0, 16, h.slot, h.off, 5)];
        assert_eq!(engine.submit(&ops), 1);
        let mut out = Vec::new();
        wait_for_completions(engine.as_ref(), 1, &mut out);
        assert!(out[0].result.is_err());
    }

    #[test]
    fn batch_of_many_round_trips() {
        let (engine, arena) = test_engine();
        const N: usize = 256;
        let mut handles: Vec<crate::io_engine::zero_copy::BufHandle> = Vec::with_capacity(N);
        for i in 0usize..N {
            // We can only lease 32 at a time: flush the wave BEFORE
            // leasing the next handle, so the arena never over-subscribes.
            if handles.len() == 32 {
                let wave: Vec<IoOp> = handles
                    .iter()
                    .enumerate()
                    .map(|(j, h)| {
                        IoOp::write(
                            0,
                            (i - 32 + j) as u64 * 4096,
                            4096,
                            h.slot,
                            h.off,
                            (i - 32 + j) as u64,
                        )
                    })
                    .collect();
                assert_eq!(engine.submit(&wave), 32);
                let mut out = Vec::new();
                wait_for_completions(engine.as_ref(), 32, &mut out);
                for h in handles.drain(..) {
                    arena.release(h);
                }
            }
            let h = arena
                .lease(4096)
                .expect("arena has 32 slots; waves respect backpressure");
            handles.push(h);
        }
        if !handles.is_empty() {
            let start = N - handles.len();
            let wave: Vec<IoOp> = handles
                .iter()
                .enumerate()
                .map(|(j, h)| {
                    IoOp::write(
                        0,
                        (start + j) as u64 * 4096,
                        4096,
                        h.slot,
                        h.off,
                        (start + j) as u64,
                    )
                })
                .collect();
            assert_eq!(engine.submit(&wave), wave.len());
            let mut out = Vec::new();
            wait_for_completions(engine.as_ref(), wave.len(), &mut out);
        }
        assert!(engine.stats().completed.load(Ordering::Relaxed) >= N as u64);
    }
}
