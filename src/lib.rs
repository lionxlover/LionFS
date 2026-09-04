#![allow(
    clippy::module_inception,
    clippy::too_many_arguments,
    clippy::only_used_in_recursion,
    clippy::needless_range_loop,
    unused_assignments
)]

pub mod allocator;
pub mod api;
pub mod btree;
pub mod cache;
pub mod common;
pub mod debug;
pub mod directory;
pub mod disk;
pub mod extents;
pub mod file;
pub mod fs;
pub mod inode;
pub mod integrity;
pub mod mount;
pub mod object;
pub mod ondisk;
pub mod optimizer;
pub mod path;
pub mod pool;
pub mod recovery;
pub mod security;
pub mod telemetry;
pub mod transaction;
pub mod utils;
pub mod worker;

// --- LionFS 2.0 (LFS-RFC-002 + RFC-003 cross-platform) ---------------------

/// Platform abstraction layer: the single place Linux/macOS/Windows
/// differences are visible (positioned I/O, fsync flavors, geometry
/// probing, CSPRNG, errno constants, waker).
pub mod pal;

/// I/O engine & concurrency model (Pillar I): batched async submission,
/// per-core shards, lock-free queues, group commit, zero-copy buffers,
/// and the Linux io_uring fast path (feature `io_uring`).
pub mod io_engine;

/// 128-bit volume addressing & packed extent records (Pillar II).
pub mod addressing;

/// Write-optimized B-epsilon extent index (Pillar II).
pub mod beepsilon;

/// HAMT inode-namespace index (Pillar II).
pub mod hamt;

/// RCU, seqlock, and epoch-based reclamation primitives (Pillar I).
pub mod rcu;

/// Hardware-aware media policies (Pillar IV): ZNS zone-append, SMR bands,
/// universal alignment, CXL PMEM tiering.
pub mod media;

/// Compression & deduplication pipeline (Pillar V): tiered codecs,
/// punch-through escape, FastCDC chunking, bounded dedup index.
pub mod pipeline;

/// Platform-neutral VFS operations surface, from which the FUSE bridge
/// (unix) and future WinFsp bridge (Windows) hang.
pub mod vfs;

pub mod kernel;



// --- LionFS 3.0 (LFS-RFC-004, "the unlimited release") ----------------------

/// 256-bit dynamic addressing, the capacity plane (RFC-004 §3): the
/// opt-in `Wide` namespace for fabric pools whose member count and
/// logical span are unbounded by a single host's lifetime. Compact
/// 128-bit volumes embed losslessly; see [`addressing::va256`].
pub use addressing::va256;

/// QoS & multi-tenancy (RFC-004 §4): IO priority classes, dual token
/// buckets, per-namespace quotas with grace periods, and weighted fair
/// queuing in virtual time.
pub mod qos;

/// Small-file record journal (RFC-004 §5): the LMDB-style batch path
/// that turns three scattered device ops per small write into one
/// sequential log write, with CRC-checked replay and torn-tail
/// recovery.
pub mod recordlog;

/// Copy-GC & space reclamation (RFC-004 §6): the Rosenblum-Ousterhout
/// cost/benefit planner (extended with wear leveling and panic-mode
/// watermarks) that turns CoW stale extents back into free space.
pub mod gc;

/// Guardian: the userspace autonomous-operations agent (RFC-004 §7) --
/// ransomware entropy watch, Weibull drive-failure prediction,
/// workload classification, and the advisory bus. Runs strictly
/// out-of-band; the data path stays deterministic.
pub mod guardian;

/// Migration & foreign-filesystem import (RFC-004 §9): magic-byte
/// detection of source filesystems, the SHA-256 verification manifest,
/// and the import-strategy planner (tar-stream / per-file / raw-block).
pub mod migrate;

/// Container & VM awareness (RFC-004 §10): image-layer
/// content-addressable storage with refcounted sharing, and the
/// virtiofs passthrough policy table.
pub mod container;

/// Prometheus text-exposition metrics registry with log-linear
/// latency histograms (RFC-004 §8).
pub use telemetry::prometheus;

// --- LionFS 3.1 (Phase 8: the wiring) ---------------------------------------

/// The Phase 8 wiring (RFC-004 §15): every 3.0 policy layer onto the
/// live engine path it governs -- QoS admission into the shard
/// dispatcher, WFQ into group commit's batch pick, the record
/// journal onto the small-write path, the GC planner's execution
/// loop, retention into the snapshot daemon, rebalance into the
/// pool manager, Guardian + Prometheus onto the sockets, key
/// envelopes into mkfs/mount, and migration onto the real ustar
/// stream. Every seam is a pure step function over caller-supplied
/// time.
pub mod wiring;

/// The deterministic simulator (Phase 8, ②): seeded universes, a
/// simulated clock, and the full-stack crash simulator that injects
/// power cuts at deterministic points and verifies replay
/// convergence as an invariant. Same seed, same universe,
/// bit-for-bit, on every platform.
pub mod sim;
