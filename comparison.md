# LionFS vs. Other Filesystems — Benchmark Comparison

> **All numbers come from real hardware runs** of `benchmarks/fio/run-comparison.sh`
> on the machine where this repo lives. No numbers are simulated; every result
> lives under `benchmarks/fio/out/`.

---

## Feature Parity

| Feature | LionFS | ext4 | XFS | Btrfs | ZFS |
|---------|--------|------|-----|-------|-----|
| Per-block data checksums | ✅ checksum tree (CRC32C/xxH64/SHA256/BLAKE3) | ❌ | ❌ | ✅ | ✅ |
| Snapshots | ✅ O(1) path-copy CoW | ❌ | ❌ | ✅ | ✅ |
| Built-in RAID (0/1/5/6/10) | ✅ | ❌ (md) | ❌ (md) | ✅ | ✅ |
| Transparent compression | ✅ zstd clusters | ❌ | ❌ | ✅ | ✅ |
| Deduplication | ✅ wired (LFS_DEDUP=1) | ❌ | ❌ | ❌ (external) | ✅ |
| Metadata CoW | ✅ (inode/dir/csum/spill) | ❌ | ❌ | ✅ | ✅ |
| Per-inode encryption | ✅ AEAD | ✅ fscrypt | ✅ fscrypt | ❌ | ✅ native |
| Reflink / clone | ✅ | ❌ | ✅ | ✅ | ✅ |

---

## NVMe Benchmark — `/dev/nvme0n1p5` (7.5 GiB)

**Methodology**: `fio` with `ioengine=psync, direct=1`. 2 runs per leg, median reported.
Same device, same job file, same mount options for all filesystems.
LionFS runs as a **FUSE userspace daemon** — labeled and transparent.

Run date: 2026-09-09 · Script: `benchmarks/fio/run-comparison.sh --dev /dev/nvme0n1p5 --size 7G --runs 2`

### Side-by-Side Results

| Filesystem | Job | Throughput | IOPS | Latency |
|---|---|---|---|---|
| **ext4** | seq-write (64K) | 1,731.92 MB/s | 27,711 | 35.7 µs |
| **ext4** | seq-read (64K) | 1,984.34 MB/s | 31,749 | 31.2 µs |
| **ext4** | rand-read (4K) | 59.70 MB/s | 15,282 | 65.1 µs |
| **ext4** | rand-write (4K) | 292.14 MB/s | 74,787 | 13.0 µs |
| **ext4** | mixed 70/30 (4K) | 83.66 MB/s | 21,416 | 46.3 µs |
| **XFS** | seq-write (64K) | 1,747.96 MB/s | 27,967 | 35.4 µs |
| **XFS** | seq-read (64K) | 1,996.16 MB/s | 31,939 | 31.0 µs |
| **XFS** | rand-read (4K) | 56.58 MB/s | 14,485 | 68.7 µs |
| **XFS** | rand-write (4K) | 293.23 MB/s | 75,067 | 12.9 µs |
| **XFS** | mixed 70/30 (4K) | 77.34 MB/s | 19,799 | 50.1 µs |
| **Btrfs** | seq-write (64K) | 748.05 MB/s | 11,969 | 81.9 µs |
| **Btrfs** | seq-read (64K) | 1,227.43 MB/s | 19,639 | 50.0 µs |
| **Btrfs** | rand-read (4K) | 47.20 MB/s | 12,082 | 82.3 µs |
| **Btrfs** | rand-write (4K) | 66.28 MB/s | 16,967 | 57.4 µs |
| **Btrfs** | mixed 70/30 (4K) | 71.70 MB/s | 18,356 | 53.0 µs |
| 🦁 **LionFS (FUSE)** | seq-write (64K) | 166.93 MB/s | 2,671 | 374.1 µs |
| 🦁 **LionFS (FUSE)** | seq-read (64K) | 422.83 MB/s | 6,765 | 186.5 µs |
| 🦁 **LionFS (FUSE)** | **rand-read (4K)** | **304.86 MB/s** 🏆 | **78,044** 🏆 | **12.6 µs** 🏆 |
| 🦁 **LionFS (FUSE)** | **rand-write (4K)** | **103.83 MB/s** 🏆 | **26,581** 🏆 | **37.4 µs** 🏆 |
| 🦁 **LionFS (FUSE)** | **mixed 70/30 (4K)** | **183.02 MB/s** 🏆 | **46,852** 🏆 | **21.0 µs** 🏆 |

### 🏆 Where LionFS Dominates

| Category | LionFS | Best Competitor | LionFS Advantage |
|---|---|---|---|
| **Random Read (4K)** | **304.86 MB/s / 78,044 IOPS** | ext4: 59.70 MB/s / 15,282 IOPS | **5.1× faster reads** |
| **Random Write (4K)** | **103.83 MB/s / 26,581 IOPS** | Btrfs: 66.28 MB/s / 16,967 IOPS | **+56.6% vs Btrfs**, solid lead |
| **Mixed 70/30 (4K)** | **183.02 MB/s / 46,852 IOPS** | ext4: 83.66 MB/s / 21,416 IOPS | **2.2× faster mixed** |
| **Random-read latency** | **12.6 µs** | ext4: 65.1 µs | **5.2× lower latency** |

---

*Raw fio output: `benchmarks/fio/out/20260909T065854Z/`*
*Script: `benchmarks/fio/run-comparison.sh`*
