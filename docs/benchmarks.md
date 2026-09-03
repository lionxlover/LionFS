# LionFS Benchmarking Status

An earlier version of this document described an elaborate "dual-layer benchmarking architecture" -- eBPF tracing, Grafana pipelines, 256-thread lock-contention testing. None of that matched the actual code. This version reports what was actually measured, in this repository, with commands you can re-run.

## What these numbers are -- and are NOT

The development container for this work has **no `/dev/fuse`**, so mounting LionFS and running `fio` against it is impossible here. All numbers below come from `lfs_ioperf` (`tools/ioperf/`), an **in-process harness that drives the real I/O core** -- `FileManager` reads/writes, the bitmap allocator, the checksum tree, the transaction layer, and the RAID engine -- against image files on tmpfs.

They measure **user-space CPU cost of the LionFS I/O path**. They are **NOT comparable to fio-on-mount numbers**: there is no FUSE round trip, no kernel page cache, and no syscalls in either direction. They are valid for before/after comparisons **within this harness only**, which is exactly how every phase's per-change attribution below was produced. When LionFS is someday benchmarked mounted, those numbers belong in a separate section, measured side by side with the comparison filesystems on identical hardware -- anything else is marketing.

## Environment

| Item | Value |
|---|---|
| CPU | 2 vCPU (shared container; Intel Xeon) |
| RAM | 4 GB |
| Storage | `/tmp` on tmpfs (numbers reflect userspace CPU cost, not device throughput) |
| Kernel | 5.10.134 x86_64 |
| rustc | 1.98.0 (2026-08-18), `cargo build --release` |
| Benchmark | `lfs_ioperf` at the commit recorded in `git log` |

Run-to-run variance on this shared container is high (single batches drift up to ±15%), so every before/after comparison below was measured as **interleaved A/B runs (3 rounds, medians)** to cancel drift. Single runs are not trustworthy here.

## How to reproduce

```
cargo build --release --bin lfs_ioperf
./target/release/lfs_ioperf --secs 3                  # single-device suite
./target/release/lfs_ioperf --profile raid5 --devices 6 --secs 3
./target/release/lfs_ioperf --profile raid6 --devices 6 --secs 3
./target/release/lfs_ioperf --compress                # corpus ratio + level sweep
```

Raw outputs for the baseline (pre-Phase-1), per-phase results, and the final re-benchmark are checked in under `benches/results/`.

## Results: single device (all phases together)

32 MiB working region, 4 KiB units for random / 64 KiB for the plan's sequential profile, checksums ON (XxHash64 per block, checksum tree), tx-buffered like the FUSE path between fsyncs. Medians of interleaved runs.

| pattern | baseline (P0c) | final (P5) | delta | extent fragments |
|---|---:|---:|---:|---:|
| seq4k-write-fresh | 528 MiB/s | 569 MiB/s | +7.7% | 8192 -> 8 |
| seq4k-write | 882 MiB/s | 1033 MiB/s | +17.1% | 7 |
| seq4k-read | 1194 MiB/s | 1441 MiB/s | +20.7% | 7 |
| seq64k-write-fresh | 561 MiB/s | 832 MiB/s | +48.4% | 8192 -> 8 |
| seq64k-write | 930 MiB/s | 1060 MiB/s | +14.0% | 7 |
| seq64k-read | 1197 MiB/s | 1454 MiB/s | +21.4% | 7 |
| rand4k-read | 1120 MiB/s | 1323 MiB/s | +18.1% | 8 |
| rand4k-write | 824 MiB/s | 831 MiB/s | +0.8% | 8191 (random layout: expected) |

The "fragments" column is the honest star of this table: a sequentially written 32 MiB file went from **8192 extent fragments (one per block, because checksum-tree node allocations interleaved with 1-block data allocations) to 8** (speculative extent sizing + metadata zoning, P1). Every read that used to walk the extent-spill B-tree now resolves inline, which is where most of the read improvement comes from.

`rand4k-write` is honestly ~flat: random 4 KiB writes to a fresh file fragment by design (one extent per first-touch block); the steady-state RMW path benefits from P1's zero-copy but the checksum-tree insert dominates and is unchanged in cost.

### Per-phase attribution (interleaved A/B medians)

- **P1.1 zero-copy block paths**: seq reads +3.1%, seq writes +1.8–3.1% (removed per-block `Vec` copies).
- **P1.2 locality + frontier cursor**: within noise on this harness (checksum insert dominates ~45% of write cost per a `--no-checksums` comparison); the change is asymptotic (O(1) frontier scans) and about physical locality, which tmpfs cannot show.
- **P1.3 speculative sizing + metadata zoning**: fragments 8192 -> 8; seq64k-write-fresh +40%; reads +18–21%.
- **P1.4 Markov readahead**: **negative result** -- wired per the plan, measured -48%..-51% on every read pattern (the per-read LRU insert reintroduces the 4 KiB copy P1.1 removed; prefetches are pure overhead when reads hit the tx dirty map). Ships **default OFF** (`LFS_READAHEAD=1` to enable). It may pay off on a real mount with cold reads; that hypothesis is untested here.
- **P2**: no throughput claim (geometry checks, chunk rationale, alignment counters).
- **P3 incremental parity**: see RAID section.
- **P4 compression clusters**: see compression section.

## Results: RAID pools (P2/P3)

The parity cost lives in the COMMIT (journal + fsync + per-block apply through the RAID engine), so the harness measures the tx-buffered write pass, the commit, and a post-commit read-back separately (`--profile raid5|raid6 --devices N`).

**Phase 2 measurement** (the question the plan asked: how often are parity writes unaligned?): 100.0% of parity writes covered a partial chunk, forcing a full stripe-row read on every single one (2.00 row reads/write on 4-dev RAID5, 3.00 on 6-dev RAID6). That measurement is what justified P3.

**Phase 3** (incremental RMW parity; same-harness A/B via `LFS_PARITY_FULL=1`):

| workload (commit) | full recompute | incremental | delta |
|---|---:|---:|---:|
| raid5-6dev, random 4 KiB writes | 367 MiB/s | 629 MiB/s | **+71.0%** |
| raid6-6dev, random 4 KiB writes | 176 MiB/s | 286 MiB/s | **+62.1%** |
| raid5-4dev, sequential 64 KiB (vs pre-P3 binary) | 251 MiB/s | 306 MiB/s | +21.9% |
| raid6-6dev, sequential 64 KiB (vs pre-P3 binary) | 28 MiB/s | 141 MiB/s | +394.7%* |

\* includes a GF(256) hot-path fix (precomputed multiplication table) that also speeds the full-recompute path; the RAID6 number is not purely the algorithmic change.

100% of parity writes are now served incrementally (0.00 row reads/write). Journal replay deliberately keeps the full-recompute path (the incremental update is not idempotent under replay of a partially-applied transaction); see the P3 commit message for the crash-safety analysis. Non-parity profiles are unchanged (within noise).

RAID6's commit cost remains high relative to RAID5: Q-syndrome math is scalar GF(256) over every byte. The table optimization helped; SIMD would help more. Unstartted work, honestly labeled.

## Results: compression clusters (P4)

Mixed corpus (deliberately not artificially-repetitive, per the plan): 40% repeating records / 35% dictionary text / 25% PRNG bytes; 8 MiB logical. Measured from the allocator bitmap (blocks actually consumed), not inferred.

```
write (64 KiB calls, cluster RMW):  186 MiB/s
sequential read:                    805 MiB/s (byte-identical verified)
random 4 KiB reads (2 MiB LRU):     26.6k ops/s
space: 706 physical blocks for 2048 logical -> ratio 2.90x (34.5% of original)
```

zstd level tradeoff (ratio vs write throughput), `mount_lfs -o zstd_level=N`:

| level | ratio | write |
|---|---:|---:|
| 1 | 2.86x | 476 MiB/s |
| 3 (default) | 2.90x | 407 MiB/s |
| 6 | 2.96x | 105 MiB/s |
| 9 | 2.98x | 60 MiB/s |

The tradeoff is real: +0.12x ratio from level 3 to 9 costs 6.8x the CPU. Level 3 stays the default.

**Honest negatives and tradeoffs:**
- Random small writes into compressed data are whole-cluster read-modify-write (128 KiB decompress + recompress per 4 KiB write, worst case). Same class of tradeoff Btrfs makes; documented in `src/file/cluster.rs`.
- Compression + encryption on one inode is rejected as unsupported (explicit error, not silent misbehavior).
- Compressed inodes do not use the per-block checksum tree; corruption detection is zstd frame decode failure.

## Microbenchmarks (`cargo bench`, via criterion)

`benches/{btree,allocator,io}_bench.rs` exist as criterion harness skeletons. They are not currently measuring the real operations their names suggest. Turning them into real benchmarks is unstarted work, not something this pass changed.

## Cross-filesystem comparison

**There is none, and none is claimed.** No ext4/XFS/Btrfs/ZFS was built, mounted, or run on this hardware as part of this work. `docs/comparison.md` previously contained a fabricated IOPS table attributed to hardware that never ran this code; it has been removed. A credible comparison requires LionFS mounted normally, fio, real NVMe, and the comparison filesystems configured identically -- same hardware, same run.
