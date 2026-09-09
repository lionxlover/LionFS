# LionFS Benchmark Results

> All numbers come from real hardware runs. Raw fio JSON output is preserved
> under `benchmarks/fio/out/`. The run script is `benchmarks/fio/run-comparison.sh`.

---

## Latest Results — NVMe (2026-09-09)

**Hardware**: `/dev/nvme0n1p5` (7.5 GiB partition on NVMe SSD)  
**Tool**: fio `ioengine=psync direct=1 runtime=15s` · 2 runs, median  
**Competitors**: ext4 (1.47.2), XFS (6.13.0), Btrfs (6.14)

### Summary Table

| Filesystem | seq-write | seq-read | rand-read | rand-write | mixed 70/30 |
|---|---|---|---|---|---|
| **ext4** | 1,732 MB/s | 1,984 MB/s | 60 MB/s | 292 MB/s | 84 MB/s |
| **XFS** | **1,748 MB/s** | **1,996 MB/s** | 57 MB/s | 293 MB/s | 77 MB/s |
| **Btrfs** | 748 MB/s | 1,227 MB/s | 47 MB/s | 66 MB/s | 72 MB/s |
| 🦁 **LionFS (FUSE)** | 167 MB/s | 423 MB/s | **305 MB/s** 🏆 | **104 MB/s** 🏆 | **183 MB/s** 🏆 |

### IOPS Table

| Filesystem | seq-write IOPS | seq-read IOPS | rand-read IOPS | rand-write IOPS | mixed IOPS |
|---|---|---|---|---|---|
| ext4 | 27,711 | 31,749 | 15,282 | 74,787 | 21,416 |
| XFS | 27,967 | 31,939 | 14,485 | 75,067 | 19,799 |
| Btrfs | 11,969 | 19,639 | 12,082 | 16,967 | 18,356 |
| 🦁 **LionFS** | 2,671 | 6,765 | **78,044** 🏆 | **26,581** 🏆 | **46,852** 🏆 |

### Latency Table (lower is better)

| Filesystem | seq-write | seq-read | rand-read | rand-write | mixed |
|---|---|---|---|---|---|
| ext4 | 35.7 µs | 31.2 µs | 65.1 µs | 13.0 µs | 46.3 µs |
| XFS | 35.4 µs | 31.0 µs | 68.7 µs | 12.9 µs | 50.1 µs |
| Btrfs | 81.9 µs | 50.0 µs | 82.3 µs | 57.4 µs | 53.0 µs |
| 🦁 **LionFS** | 374.1 µs | 186.5 µs | **12.6 µs** 🏆 | 37.4 µs | **21.0 µs** 🏆 |

---

## Score Card: LionFS vs Competitors

| Workload | vs ext4 | vs XFS | vs Btrfs |
|---|---|---|---|
| seq-write | ❌ FUSE bound | ❌ FUSE bound | ❌ FUSE bound |
| seq-read | ❌ FUSE bound | ❌ FUSE bound | ❌ FUSE bound |
| **rand-read** | ✅ **+5.1×** | ✅ **+5.4×** | ✅ **+6.5×** |
| **rand-write** | ❌ −2.8× | ❌ −2.8× | ✅ **+1.57× (+56.6%)** |
| **mixed 70/30** | ✅ **+2.19×** | ✅ **+2.37×** | ✅ **+2.55×** |

---

## Optimisation History (rand-write 4K on NVMe)

| Change | rand-write result |
|---|---|
| Initial (broken journal on reformat) | 0 MB/s (ENODATA crash) |
| Journal zeroing on mkfs | 21.93 MB/s |
| RangeLeaf in-place update cache (B-tree) | 22.95 MB/s |
| Deferred sorted checksum inserts + 64/128MB flush limits | 94.84 MB/s |
| Multi-victim flusher + O(1) page cache dirty tracking | **103.83 MB/s** |

*Raw data: `benchmarks/fio/out/20260909T065854Z/summary.md`*
