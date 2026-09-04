# LionFS

**LionFS** is a from-scratch, high-performance, self-healing universal
file system written in Rust, targeting **line-rate throughput,
extreme scalability, autonomous resilience, QoS'd multi-tenancy, and
cross-platform operation** (Linux, macOS, Windows) from one code base.
This README describes what the tree actually implements; it
deliberately does not advertise features that aren't there.

**Status: 3.1 pre-alpha, unverified on real hardware.** The engine
compiles and its test suite (**713 tests**) is green on Linux (with
and without io_uring); macOS/Windows are compile-clean by
construction (the PAL carries all platform differences) and exercised
in CI. Before trusting it with data: build it, run `cargo test`,
exercise it against real workloads -- then run
`lfs_simulate sweep` and watch every crash point pass.

## Architecture at a glance

One request path, layered top to bottom; every platform difference is
confined to the PAL, and every 3.0 policy layer sits on the path it
governs through a `src/wiring/` seam:

```mermaid
flowchart TB
    APP["Applications"] --> FUSE["FUSE bridge (Linux, macFUSE)"]
    APP --> WINFSP["WinFsp bridge (Windows, RFC-003 binding design)"]
    FUSE --> VFS["vfs: the VfsOps surface"]
    WINFSP --> VFS
    VFS --> WIRE["wiring: 7 seams (qos_gate, small_write, gc_loop, retention_daemon, telemetry_bridge, key_flow, tar_stream)"]
    WIRE --> ENGINE["io_engine: per-core shards, MPMC queues, group commit"]
    ENGINE --> INDEX["B-epsilon extent index, HAMT namespace"]
    ENGINE --> PAL["PAL: positioned I/O, sync flavors, geometry, CSPRNG, wakers"]
    PAL --> URING["io_uring backend (Linux, feature-gated)"]
    PAL --> THREADED["Threaded backend (portable floor)"]
    URING --> MEDIA["Media tiers: SSD, ZNS, SMR, CXL-PMEM"]
    THREADED --> MEDIA
    GUARDIAN["Guardian advisory bus (strictly out-of-band)"] -.-> WIRE
    SIM["sim: deterministic crash simulator"] -.-> WIRE
```

How one write traverses the 3.1 wiring, end to end:

```mermaid
flowchart LR
    REQ["VFS write"] --> QUOTA["Quota early-reject"]
    QUOTA --> BUCKETS["Dual token buckets (bytes/s + ops/s)"]
    BUCKETS --> ROUTE{"Write size 4032 B or less?"}
    ROUTE -->|yes| JOURNAL["Record journal: one sequential log write"]
    ROUTE -->|no| COW["Ordinary CoW data path"]
    JOURNAL --> OVERLAY["Read-your-write overlay"]
    OVERLAY --> DRAIN["Checkpoint drain into the B-epsilon tree"]
    DRAIN --> COMMIT["Group commit batch, WFQ pick (weights 8:4:1)"]
    COW --> COMMIT
    COMMIT --> BARRIER["PAL durability barrier"]
    BARRIER --> CUT{"sim: power cut at op index?"}
    CUT -->|no crash| DONE["Write durable"]
    CUT -->|crash| REPLAY["Replay: prefix property, overlay convergence"]
```

The GC loop runs the same CoW path as a Bulk-class background
circuit; the retention and rebalance daemons tick on caller-supplied
time; the telemetry bridge exports 19 bounded series built from every
layer's A/B counters.

## The 2.0 architecture (LFS-RFC-002, implemented here)

Five pillars, each grounded in the 1.x substrate:

| Pillar | What landed | Where |
|---|---|---|
| **I. I/O engine** | io_uring backend (registered files, batched enter, kernel-side waits), portable threaded floor, per-core shards, Vyukov MPMC queues, group commit (5 ms/1 MiB windows), zero-copy lease-exclusive buffer arena | `src/io_engine/` |
| **II. Scalability** | 128-bit volume addressing + packed 16-byte extents, B-epsilon extent index (buffered leaves, 25% padding), persistent HAMT namespace, v3 inode with **inline small files** (≤4032 B stored in metadata: one read, zero data blocks) and tail packing | `src/addressing/`, `src/beepsilon/`, `src/hamt/`, `src/ondisk/inode_v3.rs` |
| **III. Reliability** | Five-state mount recovery machine, dual-speed checksums (xxHash64 hot / BLAKE3-128 cold+clusters / CRC32C structural), autonomous repair planner, **generalized RS(n,k) erasure coding** (any-k-of-n, 200-round property-tested) | `src/recovery/`, `src/integrity/`, `src/pool/erasure.rs` |
| **IV. Media tiering** | ZNS zone-append policy (85% switch, WAF≈1.0 simulated), SMR band confinement + elevator sweeps + honest random-write rejection, universal 4K/16K/64K alignment with counted violations, CXL-PMEM tier + CLWB | `src/media/` |
| **V. Pipeline** | Tiered compression (probe-then-pin: LZ4/zstd-3/zstd-12/raw), punch-through escape on the 3rd RMW, FastCDC chunking (2K/8K/32K), three-level dedup index (bloom/hot-LRU/hash-tree, 0.1% RAM budget), QAT/SIMD/software selection | `src/pipeline/` |

## The 3.0 additions (LFS-RFC-004, "the unlimited release")

Eleven subsystems the 2.0 gap analysis identified as
production-blockers — all **consultative policy layers over the
unchanged 2.0 substrate** (none of them moved a floor joist):

| 3.0 pillar | What landed | Where |
|---|---|---|
| **Capacity plane** | 256-bit `WideAddr` (opt-in, mkfs-time; domain/namespace/volume/region/device/LBA + in-address byte offset for PMEM/CXL tiers), lossless 128↔256 embedding, superblock `plane` gate | `src/addressing/va256.rs` |
| **QoS & multi-tenancy** | 24 IO priority slots (Realtime/BestEffort/Bulk × 8), dual token buckets (bytes/s + ops/s, burst, lazy integer refill), per-namespace quotas with grace windows, WFQ in virtual time (declared-cost, anti-laundering) | `src/qos/` |
| **Small-file record journal** | ≤4032 B writes: 3 scattered device ops → 1 sequential log write (40 B header + payload + CRC32), torn-tail replay, `Commit`/`Checkpoint` watermark protocol | `src/recordlog/` |
| **Copy-GC** | Rosenblum-Ousterhout cost/benefit + wear leveling + panic-mode watermarks (tuned 25%/10% in 3.1), bounded plans, honest all-live refusal | `src/gc/` |
| **Guardian (autonomous ops)** | Ransomware entropy watch (Shannon + rewrite + lure EWMAs), Weibull drive-failure predictor with telemetry multipliers, 6-class workload classifier, advisory bus with escalation-safe rate limiting — **all userspace, out-of-band** | `src/guardian/` |
| **Observability** | Dependency-free Prometheus text exposition: 49-bucket log-linear latency histograms, counters/gauges, deterministic scrapes | `src/telemetry/prometheus.rs` |
| **Migration on-ramp** | 10-rule magic-byte detection (ext4/XFS/Btrfs/ZFS/F2FS/NTFS/FAT32/exFAT/HFS+/APFS), SHA-256 manifest verification protocol, strategy planner (tar-stream / per-file / raw-block-with-sign-off) | `src/migrate/` |
| **Container/VM awareness** | Image-layer CAS with refcounted sharing + hot-index pinning; virtiofs passthrough policy table (cache model / DAX / squash) | `src/container/` |
| **Key management** | PBKDF2-HMAC-SHA256 (600k iters) → KEK wraps the volume master (ChaCha20-Poly1305); per-file keys = HMAC-PRF (re-key is metadata-only); volatile-zeroizing envelope | `src/security/kdf.rs` |
| **Snapshot retention** | GFS tier budgets (tuned 48h/14d/8w/12m/7y in 3.1), additive representative selection, integer civil/ISO-week calendar | `src/fs/retention.rs` |
| **Pool evolution** | Online rebalance: capacity-proportional targets, health-discounted evacuation (Guardian-integrated), drain-to-remove, budget-sized moves on the CoW path | `src/pool/rebalance.rs` |

The complete normative architecture is
[`docs/rfc/LFS-RFC-004-unlimited.md`](docs/rfc/LFS-RFC-004-unlimited.md).

## The 3.1 wiring (Phase 8): policy layers onto the live paths

3.0's subsystems were consultative. 3.1 puts each on the path it
governs, behind a narrow seam (`src/wiring/`) whose contract is
uniform: the engine owns the thread, the wiring owns the step, every
decision is a pure function of caller-supplied time, and every
switch carries A/B counters (RFC-002 §2.4 applies to the wiring
itself).

| Wiring point | What it does | Where |
|---|---|---|
| **QoS admission + WFQ batch pick** | Quota early-reject → token buckets at the shard gate (Realtime's guarantee = metered overrun, never delay); group commit picks batches by WFQ virtual finish (weights 8:4:1) | `wiring::qos_gate` |
| **Small-write switch** | ≤4032 B writes route to the record journal (one sequential write instead of three scattered ops), read-your-write overlay, checkpoint drain into the B-epsilon tree | `wiring::small_write` |
| **GC execution loop** | census → cost/benefit plan → evacuate via the ordinary CoW path → reclaim accounting; Bulk class always, rate-unlimited in panic mode | `wiring::gc_loop` |
| **Retention + rebalance daemons** | GFS retention passes (interval-rate-limited, failed expirations retried); rebalance rounds to `is_balanced`, leaving devices drain first | `wiring::retention_daemon` |
| **Telemetry bridge** | Guardian advisory bus + every wiring layer's counters → 19 bounded Prometheus series; the telemetry and health sockets scrape one object | `wiring::telemetry_bridge` |
| **Key envelope flow** | mkfs create / mount unlock with 3-attempt lockout / passphrase rotation (master untouched) | `wiring::key_flow` |
| **Tar import session** | Real ustar stream (checksum-verified, GNU longname) → POSIX write path → SHA-256 read-back verification | `wiring::tar_stream` |
| **Deterministic crash simulator** | Seeded universes on a simulated clock; power cuts at deterministic op indexes; replay invariants (prefix property, overlay convergence) as assertions; exhaustive crash-point sweeps | `sim` + `lfs_simulate` |

Tuned defaults (the ③ pass): GC watermarks **25% kick / 10%
aggressive**, retention **48 hourly / 7 yearly**, QoS per-class rates
(RT 16 GiB/s, BE 4 GiB/s, bulk 1 GiB/s) with WFQ weights **8:4:1**.

### Capacity and service arithmetic

The default 128-bit plane addresses

$$V_{128} = 2^{128} - 1 \approx 3.4 \times 10^{38}\ \mathrm{bytes}$$

per volume, and the opt-in 256-bit `WideAddr` plane squares that to
$2^{256} \approx 1.2 \times 10^{77}$ addresses. For scale: at the
tuned Realtime ceiling of 16 GiB/s, exhausting the 128-bit LBA space
would take

$$T = \frac{2^{128}}{16 \cdot 2^{30}\ \mathrm{B/s}} \approx 6 \times 10^{20}\ \mathrm{years} \approx 4 \times 10^{10}\ \mathrm{ages\ of\ the\ universe}$$

— the plane is never the bottleneck, the channel is. Under saturation
the WFQ weights 8:4:1 entitle each class to a service share

$$\rho_i = \frac{w_i}{\sum_j w_j}, \qquad (\rho_{\mathrm{RT}},\ \rho_{\mathrm{BE}},\ \rho_{\mathrm{bulk}}) \approx (61.5\%,\ 30.8\%,\ 7.7\%)$$

and the inline small-file threshold is pure inode geometry —
$4096 - 64 = 4032$ bytes, a 4 KiB block minus the 64-byte inode v3
core — which is why a small file costs one metadata read and zero
data blocks.

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
grew from 245 to 462 in 2.0 and to **638** in 3.0), and the tools below exercise them live.

## Tools

45+ CLI binaries (see `tools/`). The 2.0 additions:

- `lfs_palinfo` — platform capability report + PAL self-test (runs on
  all three OSes; the CI artifact that proves portability).
- `lfs_engine` — the I/O engine benchmark. On this host: **707 MiB/s
  4 KiB writes, 1627 MiB/s reads through io_uring** (vs 115/117
  threaded); `1268/3605 MiB/s` at 64 KiB.
- `lfs_zns sim|report` — zone-append placement simulation (WAF 1.000,
  83% avg fill) and the media policy matrix.

The 3.0 additions:

- `lfs_guardian sim` — the full Guardian pipeline end-to-end: quiet
  workload → ransomware signature (freeze advisory at window 14) →
  degrading drive (migration advisory, ~360 days of headroom) →
  workload shift to Db (retune advisory). Zero actions touch the data
  path.
- `lfs_migrate demo|detect|plan` — the detection matrix (11/11
  checks: all ten magic rules + blank-image refusal), device
  detection, and dry-run import planning with sign-off gates.
- `lfs_gc sim` — the planner across all three watermark bands
  (healthy → background 4.5x efficiency → aggressive panic mode) plus
  the wear-leveling demonstration.
- `lfs_retention sim` — GFS verdicts over a two-week synthetic
  history (83 snapshots → 42 kept / 41 expired, tier by tier).

## Building

```bash
cargo build --release                # portable everywhere
cargo build --release --features io_uring   # Linux fast path
cargo test [--features io_uring]     # 638 tests
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
  cross-platform, 004 the unlimited release)
- [specifications/](specifications/) — the on-disk and subsystem specs
- [ROADMAP.md](ROADMAP.md) — P0-P6 phases and exit criteria
- [PORTING.md](PORTING.md) — how to port to a new platform
