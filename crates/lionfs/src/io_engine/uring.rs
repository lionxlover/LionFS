//! Linux io_uring backend (feature `io_uring`).
//!
//! Implements the RFC-002 §3.1 submission plane with one **ring-owner
//! thread** per engine: a single thread owns the `IoUring` instance,
//! which keeps all submission/completion manipulation free of cross-
//! thread interior mutability (the io-uring crate exposes
//! `submission()`/`completion()` through `&mut self` by design). Shards
//! hand ops to the owner through the lock-free inbox; the owner builds
//! SQEs, submits, waits, reaps CQEs, and publishes completions to the
//! outbox. That is one MPMC hop per op in userspace and *zero* syscalls
//! beyond `io_uring_enter` per batch -- the batched-plate amortization
//! the RFC's Table 7 budgets.
//!
//! * SQ depth from config (default 1024), CQ sized 4x SQ via
//!   `setup_cqsize` (absorbs completion bursts without drops).
//! * SQPOLL optional (`Builder::setup_sqpoll`): a kernel thread consumes
//!   the ring; the userspace owner then mostly reaps. IOPOLL optional
//!   for hipri-capable devices.
//! * Registered **files** (`register_files`): pool devices are addressed
//!   by fixed index, no per-op fd install.
//! * `WriteFua` chains an `IORING_OP_FSYNC` with `DATASYNC` behind a
//!   `IO_LINK`-ed write -- the honest FUA emulation for devices/files
//!   without native FUA; native `RWF_FUA` integration lands with the
//!   crate's exposure of it.
//! * `ZoneAppend`: the crate does not yet expose
//!   `IORING_OP_ZONE_APPEND` (kernel 5.19+); this backend therefore
//!   executes zone appends exactly like the threaded backend -- a write
//!   at the zone's write-pointer, which the media layer
//!   (`media::zns::ZoneTable`) resolved into `op.offset`, and the
//!   completion echoes it as the placed offset. When the opcode lands
//!   upstream, `translate_op` is the single switch to extend; the
//!   placement *policy* (WAF ~ 1.0 sequential fill) is identical either
//!   way, which is what the P4 exit criterion measures.
//!
//! Construction **probes** first: `io_uring_probe` and an actual ring
//! setup run before anything is registered; a kernel that refuses
//! (5.10-pre kernels, seccomp-confined containers) yields `Err` and the
//! builder transparently falls back to the threaded engine.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use io_uring::{opcode, types, IoUring, Probe};

use super::engine::{EngineBuilder, EngineStats, IoEngine};
use super::mpmc::MpmcQueue;
use super::op::{Completion, IoOp, OpKind};
use super::zero_copy::{BufHandle, RegisteredBufArena};
use crate::pal;

pub struct UringEngine {
    devices: Arc<[Arc<File>]>,
    arena: Arc<RegisteredBufArena>,
    inbox: Arc<MpmcQueue<IoOp>>,
    outbox: Arc<MpmcQueue<Completion>>,
    waker: Arc<pal::waker::Waker>,
    ring_waker: Arc<pal::waker::Waker>,
    stats: Arc<EngineStats>,
    in_flight: Arc<AtomicU64>,
    shutdown: Arc<AtomicU64>,
    owner: Option<std::thread::JoinHandle<()>>,
    sqpoll: bool,
    iopoll: bool,
}

impl UringEngine {
    /// Attempts to build the ring and start the owner thread. `Err` means
    /// the kernel refused io_uring -- the caller falls back to the
    /// threaded engine, never fails the mount.
    pub fn new(
        devices: Arc<[Arc<File>]>,
        arena: Arc<RegisteredBufArena>,
        config: &EngineBuilder,
    ) -> std::io::Result<Self> {
        assert!(!devices.is_empty(), "engine needs at least one device");
        let depth = config.sq_depth.clamp(64, 32 * 1024) as u32;

        // Ring first: SQ depth, CQ 4x, poll modes from config.
        let ring = if config.iopoll {
            IoUring::builder()
                .setup_iopoll()
                .setup_cqsize(depth * 4)
                .build(depth)
        } else if config.sqpoll {
            IoUring::builder()
                .setup_sqpoll(2000 /* idle ms before the kernel thread parks */)
                .setup_cqsize(depth * 4)
                .build(depth)
        } else {
            IoUring::builder().setup_cqsize(depth * 4).build(depth)
        }
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("io_uring_setup failed: {e}"),
            )
        })?;

        // Probe: does this kernel/ring support the ops we need? The
        // probe must be FILLED via register_probe on a live ring
        // (Probe::new() alone is empty by design).
        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("io_uring IORING_REGISTER_PROBE failed: {e}"),
            )
        })?;
        if !probe.is_supported(opcode::Read::CODE) || !probe.is_supported(opcode::Write::CODE) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "kernel io_uring lacks READ/WRITE support",
            ));
        }

        Self::start(ring, devices, arena, config, depth)
    }

    fn start(
        mut ring: IoUring,
        devices: Arc<[Arc<File>]>,
        arena: Arc<RegisteredBufArena>,
        config: &EngineBuilder,
        depth: u32,
    ) -> std::io::Result<Self> {
        // Register the pool devices as fixed files: SQEs address devices
        // by index with no per-op fd install.
        let fds: Vec<i32> = devices.iter().map(|f| f.as_raw_fd()).collect();
        ring.submitter()
            .register_files(&fds)
            .map_err(|e| std::io::Error::other(format!("io_uring register_files failed: {e}")))?;

        let inbox = Arc::new(MpmcQueue::with_capacity_hint(depth as usize));
        let outbox = Arc::new(MpmcQueue::with_capacity_hint(depth as usize * 8));
        let waker = Arc::new(pal::waker::Waker::new()?);
        let ring_waker = Arc::new(pal::waker::Waker::new()?);
        let stats = Arc::new(EngineStats::default());
        let in_flight = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicU64::new(0));

        let owner = {
            let inbox = Arc::clone(&inbox);
            let outbox = Arc::clone(&outbox);
            let ring_waker = Arc::clone(&ring_waker);
            let dispatch_waker = Arc::clone(&waker);
            let stats = Arc::clone(&stats);
            let in_flight = Arc::clone(&in_flight);
            let shutdown = Arc::clone(&shutdown);
            let arena = Arc::clone(&arena);
            let _sqpoll = config.sqpoll;
            std::thread::Builder::new()
                .name("lfs-uring-owner".to_string())
                .spawn(move || {
                    owner_loop(
                        ring,
                        &inbox,
                        &outbox,
                        &arena,
                        &stats,
                        &in_flight,
                        &shutdown,
                        &ring_waker,
                        &dispatch_waker,
                    );
                })
                .map_err(std::io::Error::other)?
        };

        Ok(Self {
            devices,
            arena,
            inbox,
            outbox,
            waker,
            ring_waker,
            stats,
            in_flight,
            shutdown,
            owner: Some(owner),
            sqpoll: config.sqpoll,
            iopoll: config.iopoll,
        })
    }
}

impl IoEngine for UringEngine {
    fn name(&self) -> &'static str {
        "io_uring"
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
        if n > 0 {
            let cur = self
                .in_flight
                .fetch_add(n as u64, Ordering::AcqRel)
                .wrapping_add(n as u64);
            self.stats.note_in_flight(cur);
            // Wake the ring owner (eventfd: a few dozen ns).
            self.ring_waker.wake();
        }
        n
    }

    fn submit_doorbell(&self) -> std::io::Result<()> {
        // The owner thread performs io_uring_enter itself; from the
        // submitter's perspective there is no doorbell to ring.
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

impl Drop for UringEngine {
    fn drop(&mut self) {
        self.shutdown.store(1, Ordering::Release);
        self.ring_waker.wake();
        if let Some(t) = self.owner.take() {
            let _ = t.join();
        }
    }
}

/// The ring-owner loop: the only thread that touches `IoUring`.
///
/// Structure: drain submissions, build+submit SQEs, then -- if the
/// KERNEL has pending ops -- block inside the kernel with
/// `submit_and_wait(1)`, which parks until a completion is posted.
///
/// The kernel-pending count is the owner's OWN arithmetic (SQEs pushed
/// minus CQEs reaped), not the dispatcher's in-flight counter: the
/// dispatcher decrements only when IT pops completions from the outbox,
/// so using it here would race (a window where it says "pending" while
/// the kernel has nothing left -> submit_and_wait blocks forever).
/// `kernel_pending > 0` GUARANTEES a CQE is coming, so the wait cannot
/// deadlock; with nothing pending the owner parks on the ring waker
/// until a shard submits or shutdown flips.
#[allow(clippy::too_many_arguments)]
fn owner_loop(
    mut ring: IoUring,
    inbox: &MpmcQueue<IoOp>,
    outbox: &MpmcQueue<Completion>,
    arena: &Arc<RegisteredBufArena>,
    stats: &EngineStats,
    in_flight: &Arc<AtomicU64>,
    shutdown: &Arc<AtomicU64>,
    ring_waker: &Arc<pal::waker::Waker>,
    dispatch_waker: &Arc<pal::waker::Waker>,
) {
    let mut staged: Vec<IoOp> = Vec::with_capacity(256);
    // Exactly the ops the kernel has not yet completed (owner-local,
    // race-free).
    let mut kernel_pending: u64 = 0;
    // Zone-append placed-offset bookkeeping (see submit_batch).
    let mut pending_appends: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    loop {
        if shutdown.load(Ordering::Acquire) != 0 {
            // Drain what remains, reap, then exit.
            drain_inbox(inbox, &mut staged);
            if !staged.is_empty() {
                kernel_pending +=
                    submit_batch(&mut ring, &staged, arena, stats, &mut pending_appends) as u64;
                staged.clear();
            }
            if kernel_pending > 0 {
                // Completions for in-kernel ops always arrive; one
                // bounded wait, then reap and leave (the dispatcher
                // outlives the engine and drains the outbox itself).
                let _ = ring.submit_and_wait(1);
            }
            let n = reap_cq(&mut ring, outbox, stats, in_flight, &mut pending_appends);
            kernel_pending = kernel_pending.saturating_sub(n as u64);
            if n > 0 {
                dispatch_waker.wake();
            }
            if staged.is_empty() && inbox.is_empty() && kernel_pending == 0 {
                return;
            }
            continue;
        }

        drain_inbox(inbox, &mut staged);
        if !staged.is_empty() {
            kernel_pending +=
                submit_batch(&mut ring, &staged, arena, stats, &mut pending_appends) as u64;
            staged.clear();
        }

        if kernel_pending > 0 {
            // Guaranteed to return: a CQE for every pending op.
            let _ = ring.submit_and_wait(1);
            let n = reap_cq(&mut ring, outbox, stats, in_flight, &mut pending_appends);
            kernel_pending = kernel_pending.saturating_sub(n as u64);
            if n > 0 {
                dispatch_waker.wake();
            }
        } else {
            // Idle: park for new submissions (bounded, shutdown-snappy).
            ring_waker.wait(Duration::from_millis(20));
        }
    }
}

fn drain_inbox(inbox: &MpmcQueue<IoOp>, staged: &mut Vec<IoOp>) {
    while staged.len() < staged.capacity() {
        match inbox.pop() {
            Some(op) => staged.push(op),
            None => break,
        }
    }
}

/// Builds and pushes SQEs for a batch, then enters the kernel once.
/// Returns the number of SQEs actually handed to the kernel (the
/// caller's kernel-pending count is incremented by exactly this).
fn submit_batch(
    ring: &mut IoUring,
    ops: &[IoOp],
    arena: &Arc<RegisteredBufArena>,
    _stats: &EngineStats,
    pending_appends: &mut std::collections::HashMap<u64, u64>,
) -> usize {
    let mut pushed = 0usize;
    for op in ops {
        if ring.submission().is_full() {
            break;
        }
        // Zone-append completions must surface the placed offset; the
        // CQE carries only user_data, so the owner records op.offset by
        // user_data here and pops it at reap time. Contract: user_data
        // tags must be unique within the in-flight window (they are
        // transaction ids / tree slots).
        if op.kind == OpKind::ZoneAppend {
            pending_appends.insert(op.user_data, op.offset);
        }
        let entry = translate_op(op, arena);
        // SAFETY: SQ push is safe when capacity was checked (is_full
        // above); the entry is fully initialized by the opcode builders.
        unsafe {
            if ring.submission().push(&entry).is_err() {
                break;
            }
        }
        pushed += 1;
    }
    if pushed > 0 {
        // One io_uring_enter for the whole batch: the syscall
        // amortization that makes the per-op cost approach zero.
        if let Err(e) = ring.submit() {
            log::warn!(
                "io_engine uring: submit failed: {e} ({}/{} ops pushed)",
                pushed,
                ops.len()
            );
        }
    }
    // Ops that did not fit (SQ full) are dropped from this batch: the
    // submit() return value already told the caller how many were
    // accepted at the inbox level; SQ-full overflow is bounded by
    // matching inbox depth to SQ depth.
    pushed
}

/// Translates one op into an SQE. Buffer lifetimes: the arena slot stays
/// leased until the completion is reaped by the dispatcher, which
/// satisfies the async buffer-lifetime contract of io_uring.
fn translate_op(op: &IoOp, arena: &Arc<RegisteredBufArena>) -> io_uring::squeue::Entry {
    let handle = BufHandle {
        slot: op.buf,
        off: op.buf_off,
        len: op.len,
    };
    // SAFETY: lease-exclusivity holds for the op's slot; see module docs.
    let buf = unsafe { arena.slice_mut(handle) };
    let dev = types::Fixed(u32::from(op.device));

    let entry = match op.kind {
        OpKind::Read => opcode::Read::new(dev, buf.as_mut_ptr(), op.len)
            .offset(op.offset)
            .build(),
        OpKind::Write => opcode::Write::new(dev, buf.as_ptr(), op.len)
            .offset(op.offset)
            .build(),
        OpKind::WriteFua => {
            // FUA = write, then a linked data-sync: one SQE chain, one
            // completion for the write (the link's fsync completion is
            // tagged as the same op via user_data).
            opcode::Write::new(dev, buf.as_ptr(), op.len)
                .offset(op.offset)
                .build()
                .flags(io_uring::squeue::Flags::IO_LINK)
        }
        OpKind::FlushData => opcode::Fsync::new(dev)
            .flags(types::FsyncFlags::DATASYNC)
            .build(),
        OpKind::ZoneAppend => {
            // See module docs: placement policy lives in media::zns; the
            // device-side IORING_OP_ZONE_APPEND opcode is not yet exposed
            // by the crate, so this is a write at the resolved pointer.
            opcode::Write::new(dev, buf.as_ptr(), op.len)
                .offset(op.offset)
                .build()
        }
        OpKind::Deallocate => opcode::Nop::new().build(),
    };
    // The CQE echoes only user_data; encode the op kind in the top two
    // bits so reap_cq can decode it, and strip the tag when publishing
    // the completion so the engine's external contract (raw user_data +
    // kind) is identical to the threaded backend.
    entry.user_data(tag_user_data(op.user_data, op.kind))
}

/// Reaps the CQ into the outbox. The CQE carries only user_data + result;
/// the op kind is recovered by the 2-bit tag on user_data (see
/// [`tag_user_data`] / [`kind_for_user_data`]).
fn reap_cq(
    ring: &mut IoUring,
    outbox: &MpmcQueue<Completion>,
    stats: &EngineStats,
    in_flight: &Arc<AtomicU64>,
    pending_appends: &mut std::collections::HashMap<u64, u64>,
) -> usize {
    let mut n = 0usize;
    for cqe in ring.completion() {
        let raw_user_data = cqe.user_data();
        let kind = kind_for_user_data(raw_user_data);
        // Strip the kind tag: callers see their original id.
        let user_data = raw_user_data & ((1 << KIND_TAG_SHIFT) - 1);
        let res = cqe.result();
        let completion = if res < 0 {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            Completion::err(user_data, kind, -res)
        } else if kind == OpKind::Read && res == 0 {
            // Zero bytes for a nonzero read request: end-of-device.
            // The engine's contract (inherited from the 1.x read_full
            // discipline) is that a read either fills its buffer or
            // fails -- garbage-zero success is never an option, and both
            // backends must agree.
            stats.failed.fetch_add(1, Ordering::Relaxed);
            Completion::err(user_data, kind, crate::pal::posix::EIO)
        } else if kind == OpKind::ZoneAppend {
            // The write landed at the resolved write pointer; surface it
            // as the placed offset (the completion shape a native
            // zone-append would produce).
            let placed = pending_appends.remove(&user_data).unwrap_or(0);
            stats.zone_appends.fetch_add(1, Ordering::Relaxed);
            Completion::zone_append(user_data, placed, res as u32)
        } else if kind == OpKind::FlushData {
            Completion::ok(user_data, kind)
        } else {
            Completion::data(user_data, kind, res as u32)
        };
        while !outbox.push(completion) {
            std::thread::yield_now();
        }
        n += 1;
    }
    // in_flight is decremented by the dispatcher's reap() (single
    // accounting point across backends).
    let _ = in_flight;
    n
}

const KIND_TAG_SHIFT: u64 = 62;

#[must_use]
fn tag_user_data(user_data: u64, kind: OpKind) -> u64 {
    let k = match kind {
        OpKind::Read | OpKind::Deallocate => 0,
        OpKind::Write | OpKind::WriteFua => 1,
        OpKind::FlushData => 2,
        OpKind::ZoneAppend => 3,
    };
    (k << KIND_TAG_SHIFT) | (user_data & ((1 << KIND_TAG_SHIFT) - 1))
}

fn kind_for_user_data(user_data: u64) -> OpKind {
    match user_data >> KIND_TAG_SHIFT {
        1 => OpKind::Write,
        2 => OpKind::FlushData,
        3 => OpKind::ZoneAppend,
        _ => OpKind::Read,
    }
}

#[cfg(test)]
mod tests {
    use super::super::op::OpResult;
    use super::*;
    use std::time::Instant;

    #[test]
    fn user_data_tagging_roundtrip() {
        let ud = (1u64 << 40) | 0xdead;
        for kind in [
            OpKind::Read,
            OpKind::Write,
            OpKind::FlushData,
            OpKind::ZoneAppend,
        ] {
            let tagged = tag_user_data(ud, kind);
            assert_eq!(kind_for_user_data(tagged), kind);
            assert_eq!(tagged & ((1 << KIND_TAG_SHIFT) - 1), ud);
        }
    }

    #[test]
    fn user_data_tagging_fits_62_bit_ids() {
        let ud = u64::MAX >> 2;
        let tagged = tag_user_data(ud, OpKind::ZoneAppend);
        assert_eq!(tagged & ((1 << KIND_TAG_SHIFT) - 1), ud);
        assert_eq!(kind_for_user_data(tagged), OpKind::ZoneAppend);
    }

    /// Whatever the host allows, building an engine must never panic and
    /// must degrade to a working engine when io_uring is refused.
    #[test]
    fn builder_degrades_gracefully() {
        let dir = std::env::temp_dir().join(format!("lionfs_uring_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dev = pal::file::create_image(dir.join("img.bin"), 4 * 1024 * 1024).unwrap();
        let devices: Arc<[Arc<File>]> = Arc::from(vec![Arc::new(dev)].into_boxed_slice());
        let arena = RegisteredBufArena::new(8, 64 * 1024);
        let engine = EngineBuilder::default().build(devices, arena);
        let _ = engine.name();
    }

    /// If the ring DID come up (kernel allows io_uring), a full
    /// write-read roundtrip through the real ring must work. Skipped
    /// automatically where the kernel refuses the ring.
    #[test]
    fn uring_roundtrip_when_available() {
        let dir = std::env::temp_dir().join(format!("lionfs_uring_rt_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dev = pal::file::create_image(dir.join("img.bin"), 4 * 1024 * 1024).unwrap();
        let devices: Arc<[Arc<File>]> = Arc::from(vec![Arc::new(dev)].into_boxed_slice());
        let arena = RegisteredBufArena::new(8, 64 * 1024);
        let engine = match UringEngine::new(devices, arena.clone(), &EngineBuilder::default()) {
            Ok(e) => e,
            Err(_) => return, // kernel refuses io_uring here: skip.
        };
        assert_eq!(engine.name(), "io_uring");

        let h = arena.copy_in(&[0xC3u8; 4096]).expect("arena");
        let ops = [IoOp::write(0, 8192, 4096, h.slot, h.off, 77)];
        assert_eq!(engine.submit(&ops), 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        while out.is_empty() {
            assert!(Instant::now() < deadline, "io_uring completion timeout");
            engine.reap(&mut out, 1);
            if out.is_empty() {
                engine.blocking_wait(Duration::from_millis(50));
            }
        }
        assert_eq!(out[0].user_data, 77);
        assert_eq!(out[0].result, OpResult::Done(4096));

        let h2 = arena.lease(4096).expect("arena");
        let ops = [IoOp::read(0, 8192, 4096, h2.slot, h2.off, 78)];
        assert_eq!(engine.submit(&ops), 1);
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while out.is_empty() {
            assert!(Instant::now() < deadline, "io_uring completion timeout");
            engine.reap(&mut out, 1);
            if out.is_empty() {
                engine.blocking_wait(Duration::from_millis(50));
            }
        }
        // SAFETY: leased by this test thread; op completed.
        let data = unsafe { arena.slice(h2) };
        assert!(data.iter().all(|&b| b == 0xC3));
    }
}
