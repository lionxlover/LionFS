# Changelog

All notable changes to the LionFS project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.0.0] - The Cross-Platform Architecture Release (LFS-RFC-002 + LFS-RFC-003)

### Added — Platform Abstraction Layer (`src/pal/`)
- **Cross-platform core**: Linux, macOS, and Windows build from one code base; the PAL is the only place platform differences exist. The Windows build pulls **zero external crates** (raw `extern "system"` FFI for `FlushFileBuffers`, `IOCTL_DISK_GET_LENGTH_INFO`, `ProcessPrng`/`RtlGenRandom`).
- Positioned I/O (pread/pwrite ↔ seek_read/seek_write), durability flavors (fdatasync / F_FULLFSYNC / FlushFileBuffers), unified geometry probing (Linux BLKGETSIZE64+BLKSSZGET+BLKPBSZGET+BLKOPTGET, macOS DKIOC*, Windows IOCTL, stat fallback), OS CSPRNG (getrandom / getentropy / ProcessPrng), and wake primitives (eventfd / self-pipe / condvar-generation).
- `libc` and `fuser` are now unix-scoped dependencies; errno and mode constants live in `pal::posix` (the FUSE wire ABI, as constants rather than libc imports).

### Added — I/O Engine (Pillar I, `src/io_engine/`)
- **io_uring backend** (feature `io_uring`): registered files, batched `io_uring_enter`, kernel-side blocking via `submit_and_wait(1)` with exact kernel-pending accounting, zone-append placed-offset bookkeeping, graceful logged fallback when the kernel refuses the ring. Measured live: 707 MiB/s 4 KiB writes / 1627 MiB/s reads (vs 115/117 threaded).
- Portable threaded engine (the correctness floor), Vyukov bounded MPMC queues, per-core shard table with splitmix64 routing, **group commit** (5 ms / 1 MiB batch windows, one flush per batch, private-tx opt-out), registered-buffer arena with dynamic lease exclusivity and counted bounce-buffer slow path.

### Added — Scalability (Pillar II)
- **128-bit volume addressing** (`src/addressing/va.rs`): volume/region/device/LBA field layout, structured ordering, checked arithmetic.
- **Packed 16-byte extent records** (`src/addressing/extent16.rs`): u48/u48/u24 + GRAN/RAW/ENC/SHARED/DEDUP flags, bytemuck-Pod, saturating end-arithmetic.
- **B-epsilon tree** (`src/beepsilon/`): buffered leaves, 2 KiB flush threshold, 25% padding, extent coalescing pass.
- **Persistent HAMT** (`src/hamt/`): 32-way bitmap-compressed trie for the inode namespace, structural sharing for RCU publication.
- **Inode v3** (`src/ondisk/inode_v3.rs`): 64-byte core + inline payloads (≤4032 B — small files become one metadata read, zero data blocks), unambiguous branch discipline on the wire, tail packer with ~4/3 write amplification.

### Added — Reliability (Pillar III)
- **Five-state mount recovery machine** (PROBE/REPLAY/CHECKPOINT/RECONCILE/WRITABLE) with audit records and fault-injection tests (convergence-after-kill).
- **Dual-speed checksums**: xxHash64 (hot pages) / BLAKE3-128 (cold + clusters, domain-separated tags) / CRC32C (structural); constant-time verification.
- **Autonomous repair planner**: quarantine → reconstruct (parity-P/PQ/mirror) → rewrite → swap-in-transaction → release; no-redundancy pools report the loss honestly.
- **Generalized Reed-Solomon RS(n,k)** (`src/pool/erasure.rs`): Vandermonde-systematic construction (right-multiplied by the top-block inverse — the MDS-correct form), any-k-of-n reconstruction, 200-round random-erasure property tests.

### Added — Media tiering (Pillar IV, `src/media/`)
- ZNS zone model: zone-append planning (85% fill switch), completion-time placed offsets, RECONCILE-from-device-report, zone reset/offline; `lfs_zns sim` shows WAF 1.000.
- SMR band allocator: per-file band confinement, elevator sweep planning, explicit `RandomWriteRejected` for random writes to host-managed bands.
- Universal alignment: 4K/16K/64K classes from probed geometry, covering allocation rounding, submission split/merge, counted violations.
- CXL-PMEM tier placement + CLWB fence path (CPUID-probed, x86-64 Linux).

### Added — Compression & dedup pipeline (Pillar V, `src/pipeline/`)
- Per-inode tiering (probe-then-pin: LZ4 / zstd-3 / zstd-12 / raw).
- Punch-through escape hatch on the third RMW against a cluster; cold re-compression after two quiescent scrub cycles.
- FastCDC content-defined chunking (2 K/8 K/32 K, gear hash, deterministic table; local-shift property tested).
- Three-level dedup index (bloom / hot LRU / on-disk tree) at the 0.1%-of-pool RAM budget; BLAKE3-128 chunk hashes.
- QAT/SIMD/software backend selection with counted rejections.

### Added — VFS & tooling
- **Platform-neutral `VfsOps` surface** (`src/vfs/`) + FUSE bridge: the engine no longer implements fuser's trait; Linux/macOS mount through the bridge, Windows/WinFsp has a complete binding design (RFC-003 §5).
- `lfs_palinfo` (platform capability report + PAL self-test), `lfs_engine` (engine benchmark), `lfs_zns` (zone simulator + policy matrix).
- Criterion benches: `beepsilon_bench`, `fastcdc_bench`.
- 3-OS CI matrix (Linux + macOS + Windows) with feature and clippy jobs.

### Changed
- The core is **free of `libc`/fuser/unix imports** (all constants via `pal::posix`, directory names via `&str`, timestamps via a neutral `TimeOrNow`).
- `mount_lfs`, the library mount path, and the C API all route through the FUSE bridge.
- `fill_random` uses the PAL CSPRNG (was `/dev/urandom`).
- Cargo: version 2.0.0, `rust-version` 1.75, unix-scoped fuser/libc, optional Linux `io-uring`, release profile with thin-LTO.
- Test suite: **245 → 462** tests (all green with and without `io_uring`).

### Fixed
- io_uring owner-loop deadlock: the wait decision now uses the owner's exact kernel-pending count instead of the dispatcher's racy in-flight counter.
- io_uring/threaded semantic parity: EOF reads are errors on both backends; zone-append completions carry placed offsets and stats on both.
- The B-epsilon/HAMT/Vyukov-queue/algebra bugs found by the new tests themselves (see the specs' "kept fixed" notes).

## [Unreleased] (1.x line, folded into 2.0.0)
- Extensive, highly-modular project directory structure; initial Phase 1 extent-based filesystem; `mkfs_lfs`, `mount_lfs`, `fsck`, `debug` utilities; zero-copy metadata via bytemuck; free-space bitmap allocator; inline extents in 256-byte inodes; dynamic directory entries; FUSE daemon for POSIX ops on Linux.

## [0.1.0] - Initial Prototype
- Proof-of-concept initialization for LionFS logic testing.
