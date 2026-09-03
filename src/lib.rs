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
