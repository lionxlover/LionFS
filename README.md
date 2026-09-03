# LionFS

**LionFS** is a from-scratch, high-performance, self-healing universal
file system written in Rust, targeting **line-rate throughput,
extreme scalability, autonomous resilience, and cross-platform
operation** (Linux, macOS, Windows) from one code base. This README
describes what the tree actually implements; it deliberately does not
advertise features that aren't there.

**Status: 2.0 pre-alpha, unverified on real hardware.** The engine
compiles and its test suite is green on Linux (with and without
io_uring); macOS/Windows are compile-clean by construction (the PAL
carries all platform differences) and exercised in CI. Before trusting
it with data: build it, run `cargo test`, exercise it against real
workloads.

## The 2.0 architecture (LFS-RFC-002, implemented here)

Five pillars, each grounded in the 1.x substrate:

| Pillar | What landed | Where |
|---|---|---|
| **I. I/O engine** | io_uring backend (registered files, batched enter, kernel-side waits), portable threaded floor, per-core shards, Vyukov MPMC queues, group commit (5 ms/1 MiB windows), zero-copy lease-exclusive buffer arena | `src/io_engine/` |
| **II. Scalability** | 128-bit volume addressing + packed 16-byte extents, B-epsilon extent index (buffered leaves, 25% padding), persistent HAMT namespace, v3 inode with **inline small files** (≤4032 B stored in metadata: one read, zero data blocks) and tail packing | `src/addressing/`, `src/beepsilon/`, `src/hamt/`, `src/ondisk/inode_v3.rs` |
| **III. Reliability** | Five-state mount recovery machine, dual-speed checksums (xxHash64 hot / BLAKE3-128 cold+clusters / CRC32C structural), autonomous repair planner, **generalized RS(n,k) erasure coding** (any-k-of-n, 200-round property-tested) | `src/recovery/`, `src/integrity/`, `src/pool/erasure.rs` |
| **IV. Media tiering** | ZNS zone-append policy (85% switch, WAF≈1.0 simulated), SMR band confinement + elevator sweeps + honest random-write rejection, universal 4K/16K/64K alignment with counted violations, CXL-PMEM tier + CLWB | `src/media/` |
| **V. Pipeline** | Tiered compression (probe-then-pin: LZ4/zstd-3/zstd-12/raw), punch-through escape on the 3rd RMW, FastCDC chunking (2K/8K/32K), three-level dedup index (bloom/hot-LRU/hash-tree, 0.1% RAM budget), QAT/SIMD/software selection | `src/pipeline/` |

**Cross-platform (LFS-RFC-003):** the platform abstraction layer
(`src/pal/`) is the only place Linux/macOS/Windows differ — positioned
I/O, fsync flavors (`fdatasync`/`F_FULLFSYNC`/`FlushFileBuffers`),
geometry probing, CSPRNG, wake primitives. The Windows build pulls
**zero external crates**. The engine implements one `vfs::VfsOps`
surface; FUSE (Linux/macFUSE) and WinFsp hang off it as bridges.

The complete normative architecture is in-repo:
[`docs/rfc/LFS-RFC-002.md`](docs/rfc/LFS-RFC-002.md) (the 2.0 RFC) and
[`docs/rfc/LFS-RFC-003-cross-platform.md`](docs/rfc/LFS-RFC-003-cross-platform.md).

## What's implemented and wired into the live path

- **Core POSIX operations** via FUSE (Linux/macOS): create, read,
  write, lookup, readdir, mkdir, unlink, rmdir, rename (incl. cross-
  directory), setattr (chmod/chown/truncate/utimens), statfs, access —
  now through the platform-neutral `VfsOps` + FUSE bridge.
- **Checksumming**: CRC32C, XxHash64, SHA-256, BLAKE3, verified on
  every read; dual-speed policy classes + per-cluster domain-separated
  BLAKE3 tags.
- **Crash consistency**: write-ahead journaling with durable fsync
  before apply, replay on mount; the five-state recovery machine
  formalizes the mount path with fault-injection tests.
- **Encryption**: AES-256-GCM / ChaCha20-Poly1305, per-file keys in
  the on-disk key tree; **CSPRNG via the PAL** (getrandom/getentropy/
  ProcessPrng — no more /dev/urandom dependency).
- **Compression**: LZ4, Zstd, Deflate per block with adaptive raw
  fallback; the 2.0 tiering engine pins codecs per inode by measured
  compressibility and latency.
- **RAID 0/1/5/6/10** with GF(256) parity, incremental RMW, degraded-
  mode reconstruction — plus generalized RS(n,k) erasure for wide
  pools.
- **POSIX permissions** on access; immutable/append-only enforcement.

## The 2.0 additions that are real, tested building blocks

io_uring engine (feature `io_uring`), MPMC queues, shards, group
commit, the arena, 128-bit addressing, Extent16, B-epsilon tree, HAMT,
RCU/seqlock, ZNS/SMR/alignment/tiering, FastCDC/dedup/tiering/punch-
through, the recovery machine, dual-speed checksums, the healer, RS
erasure, the v3 inode. Each carries unit + property tests (the suite
grew from 245 to **462**), and the tools below exercise them live.

## Tools

45+ CLI binaries (see `tools/`). The 2.0 additions:

- `lfs_palinfo` — platform capability report + PAL self-test (runs on
  all three OSes; the CI artifact that proves portability).
- `lfs_engine` — the I/O engine benchmark. On this host: **707 MiB/s
  4 KiB writes, 1627 MiB/s reads through io_uring** (vs 115/117
  threaded); `1268/3605 MiB/s` at 64 KiB.
- `lfs_zns sim|report` — zone-append placement simulation (WAF 1.000,
  83% avg fill) and the media policy matrix.

## Building

```bash
cargo build --release                # portable everywhere
cargo build --release --features io_uring   # Linux fast path
cargo test [--features io_uring]     # 462 tests
cargo bench                          # criterion: beepsilon, fastcdc, btree, allocator, io
```

See [BUILD.md](BUILD.md) and [docs/platform_support.md](docs/platform_support.md)
for per-platform details.

## Formatting and mounting

```bash
# Single device
sudo target/release/mkfs_lfs /path/to/image.bin 1024      # size in MB
sudo target/release/mount_lfs /path/to/image.bin /mnt/lion

# Multi-device RAID (RAID5 example, 4 devices)
sudo target/release/mkfs_lfs dev0.img 1024 --raid raid5 dev1.img dev2.img dev3.img
sudo target/release/mount_lfs dev0.img /mnt/lion dev1.img dev2.img dev3.img
```

## No performance claims beyond reproducible commands

Every number in this README comes from a command a reader can re-run
(`lfs_engine`, `lfs_zns sim`) on the same host — the LFS-RFC-002
honesty rule, carried forward as a first-class constraint. No
cross-filesystem comparison appears unless ext4/XFS/Btrfs/ZFS was
actually built, mounted, and measured on the same hardware in the same
run.

## Documentation

- [docs/](docs/) — architecture deep-dives: platform support, io
  engine, addressing, media tiering, pipeline, reliability, RCU
- [docs/rfc/](docs/rfc/) — the normative RFCs (002 architecture, 003
  cross-platform)
- [specifications/](specifications/) — the on-disk and subsystem specs
- [ROADMAP.md](ROADMAP.md) — P0-P6 phases and exit criteria
- [PORTING.md](PORTING.md) — how to port to a new platform
