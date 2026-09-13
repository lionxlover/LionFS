//! # I/O Engine (Pillar I)
//!
//! The submission plane of LFS-RFC-002 §3: batched, asynchronous, per-core
//! sharded device I/O with zero syscalls in steady state on the io_uring
//! backend and a portable threaded backend everywhere else.
//!
//! ## Backends (RFC-002 Table 7)
//!
//! | Backend | Platform | Steady-state cost | Notes |
//! |---------|----------|-------------------|-------|
//! | [`UringEngine`] | Linux, feature `io_uring` | ~0.1 us/op, no syscalls with SQPOLL | first choice when available |
//! | [`ThreadedEngine`] | all | thread hand-off + blocking positioned I/O | correctness floor, also the test harness |
//!
//! Selection is performed once at mount by [`EngineBuilder`] after probing
//! [`crate::pal::platform::CapabilityReport`] and attempting an actual
//! `io_uring_setup` -- a kernel that refuses the ring (old kernel, seccomp,
//! container) degrades to the threaded engine with a logged line, never a
//! mount failure.
//!
//! ## Structure
//!
//! * [`op`] -- the operation/completion descriptor vocabulary (incl.
//!   `ZoneAppend` and `FlushFua`, the Pillar IV/V orderings).
//! * [`mpmc`] -- Vyukov bounded MPMC queue, the lock-free hand-off used
//!   between shards and the reaper.
//! * [`shard`] -- per-core engine shards and the `shard_of` routing hash.
//! * [`engine`] -- the backend trait, the threaded engine, and the builder.
//! * [`uring`] -- the io_uring backend (Linux + feature).
//! * [`group_commit`] -- fsync coalescing per RFC-002 §9.4.
//! * [`zero_copy`] -- registered, aligned buffer arena + bounce-buffer
//!   slow path with an explicit perf counter.

pub mod engine;
pub mod group_commit;
pub mod mpmc;
pub mod op;
pub mod shard;
pub mod zero_copy;

#[cfg(all(target_os = "linux", feature = "io_uring"))]
pub mod uring;

pub use engine::{EngineBuilder, EngineStats, IoEngine, ThreadedEngine};
pub use group_commit::{GroupCommitBatcher, GroupCommitConfig};
pub use op::{Completion, IoOp, OpKind, OpResult};
pub use shard::{Shard, ShardTable, NUM_SHARDS_MAX};
pub use zero_copy::{BufHandle, RegisteredBufArena};

#[cfg(all(target_os = "linux", feature = "io_uring"))]
pub use uring::UringEngine;
