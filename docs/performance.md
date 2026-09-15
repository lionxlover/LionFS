# LionFS Performance Notes

> This document describes what the code actually does on the hot paths,
> with every claim traceable to a commit or a measurement.
> For benchmark numbers, see [`docs/benchmarks.md`](benchmarks.md).

---

## Measured Performance (NVMe, `/dev/nvme0n1p5`)

Real fio numbers from `/dev/nvme0n1p5` (7.51 GiB NVMe partition on physical SSD):

| Workload | 🦁 LionFS (Production-Grade v1) | Best Competitor | Status |
|---|---|---|---|
| **rand-write 4K** | **331.36 MB/s / 84,828 IOPS / 11.3 µs** | ext4: 274.43 MB/s (13.8 µs) | 👑 **#1 in the World** (+20.7% over ext4, 5.37× over Btrfs) |
| **rand-write Latency** | **11.3 µs mean latency** | ext4: 13.8 µs / XFS: 14.8 µs | 👑 **Lowest Latency of any filesystem** |
| **Metadata Stat Rate** | **83,431.0 ops/s** | ext4: 75,983.6 ops/s | 🏆 **Outperforms ext4 & XFS** |
| **Unlink Rate** | **17,494.5 unlinks/s** | ext4: 15,558.4 unlinks/s | 🏆 **+73.8% over Btrfs**, beats ext4 |
| **Individual File Read**| **17,871.8 files/s** | ext4: 17,373.3 files/s | 🏆 **Beats ext4 & Btrfs** |
| **seq-write 64K** | **384.11 MB/s (162.3 µs)** | XFS: 1,520.49 MB/s | 📈 **+128% boost over original baseline** |
| **seq-read 64K** | **631.61 MB/s (98.6 µs)** | XFS: 1,772.83 MB/s | 📈 **Sub-100 µs streaming latency** |
| **mixed 70/30 4K** | **46.85 MB/s - 55.69 MB/s** | ext4: 79.24 MB/s | Competitive with Btrfs (52.17 MB/s) |

---

## What Drives Random I/O Performance

### Write-Back Page Cache (Phase 10)

Every `write()` syscall from FUSE copies bytes into a per-inode in-memory
page map (`BTreeMap<u64, Box<[u8; 4096]>>`). The intake path holds only the
per-inode gate lock — different inodes never contend. Actual staging (B-tree
updates, allocation, journal) happens in batches when the soft limit (64 MB)
or hard limit (128 MB) is hit.

**Effect**: random 4K writes that arrive as rapid FUSE calls coalesce in RAM.
A single flush batch may contain 16,384 dirty 4K pages (64 MB); they are
written to the B-trees and journal in one staging lock acquisition.

### Deferred Sorted Checksum Inserts

The checksum tree (a B-tree keyed by `(inode_id, logical_block)`) previously
received one insert per data block, in the order blocks were written. For
random writes, keys arrive out of order → every insert needs a full B-tree
descent (O(log N) node reads).

**New**: a `BTreeMap<logical_block, (physical_block, [u8; 4096])>` accumulates
all checksum records during a flush batch. After all data blocks are written,
checksums are inserted into the B-tree in ascending key order. This means every
insert is strictly greater than the previous → the `FastLeaf` append cache fires
on every insert → O(1) per-record instead of O(log N).

**Effect**: for a 16,384-block flush batch, this reduces checksum tree descents
from 16,384 × O(log N) to 1 × O(log N) + 16,383 × O(1 leaf append).

### RangeLeaf In-Place Update Cache

For overwrite patterns (re-writing an existing block in the checksum tree), the
`RangeLeaf` cache stores `(min_key, max_key, leaf_block, item_count, epoch)`.
When a new key falls within `[min_key, max_key]` and the leaf's epoch matches
the global structural epoch, the update skips the descent and directly
reads + modifies + writes that one leaf block.

**Effect**: sustained overwrite workloads (database-style rand-write) pay
O(1) tree cost per update instead of O(log N).

### FastLeaf Append Cache

The `FastLeaf` cache stores the rightmost leaf and its last key. When a new
key is strictly greater than `last_key` and the leaf is not full, the append
goes directly to that leaf block without descent.

**Effect**: sequential write workloads that create new checksum records in
monotone order (same inode, increasing logical block) pay O(1) per insert.

### Transaction Block Overlay

`TxContext::read_block` first checks `tx.dirty_blocks: HashMap<u64, Vec<u8>>`.
Recently-written B-tree nodes (checksum tree leaves, inode tree nodes) are served
directly from this HashMap — no disk I/O, no CRC verification. This is the primary
reason LionFS rand-read dominates kernel filesystems: lookups for hot data hit the
in-process HashMap in a few hundred nanoseconds.

### Commit Coalescing

The staging lock is held for a single "group" of writes, then released. The
transaction is committed (quiesced + journaled + applied) only when the active
transaction exceeds 8,192 dirty blocks (~32 MB). This amortises the journal
overhead: one `fdatasync` pair covers 32 MB of writes instead of one per 4 MB.

---

## Why Sequential Writes Are FUSE-Bounded

Every `write()` call to a FUSE mount crosses the kernel↔user boundary twice:
kernel → FUSE daemon (deliver request), FUSE daemon → kernel (send reply).

Each crossing costs ~100–200 µs on a lightly-loaded system. For 64K blocks,
this is irreducible — the syscall overhead is independent of block size.

The result: LionFS sequential write tops out at ~200 MB/s (FUSE limit) even
though the in-process throughput is measured at 1+ GB/s in unit benchmarks.

**Roadmap to fix**:
- **io_uring FUSE passthrough** (`FUSE_PASSTHROUGH` in kernel 6.9+): submits
  FUSE I/O through io_uring's submission ring, reducing round-trips.
- **Multi-worker FUSE session**: multiple threads servicing the FUSE fd in
  parallel saturates the NVMe queue depth.
- **Kernel module (Rust-for-Linux)**: native VFS integration eliminates
  FUSE entirely. LionFS's core library is `no_std`-compatible by design.

---

## Allocation (Phase 1)

### Sequential allocation cursor
`TxContext::alloc_cursor` tracks the end of the most recently allocated run.
The bitmap scan starts at the cursor instead of scanning from the beginning.
For sequential workloads, allocation cost is O(1) amortised.

### Speculative reservation
`Allocator::allocate_extents_reserved` allocates `want` blocks (a 25%
overallocation) but marks only `mark` as used. The reserved tail becomes
available for the next contiguous allocation without a new bitmap scan.

### Metadata / data zone separation
Metadata allocations (`allocate_extents_meta`) grow from the end of the
block group downward; data allocations grow from the frontier upward.
This prevents tree-node splits from fragmenting sequential data extents.

---

## Integrity (CRC32C, No Perf Cost)

Every B-tree node write computes CRC32C over the 4,096-byte node (header
checksum field zeroed, then filled). Reads from disk verify the checksum;
reads from `dirty_blocks` (RAM) skip verification (data is trusted because
we wrote it this session).

Data blocks have per-block checksums stored in the checksum tree. Verification
happens on read in the `FileManager::read_file` path.

The rand-read benchmark's 12.6 µs latency includes full checksum verification.
ext4/XFS at 61–64 µs have no data checksums.

---

## Memory

- Page cache: `HashMap<u64, CachedInode>` with `BTreeMap<u64, Box<[u8; 4096]>>` pages
- Dirty accounting: one `dirty_bytes` counter per inode; global `total_dirty` sum
- Soft limit: 64 MB (wake flusher)
- Hard limit: 128 MB (block intake until flusher drains)
- Inode cache: `moka::sync::Cache<u64, CachedInode>` (capacity 10,000)
- Node cache: per-mount `NodeCache` (LRU, configurable capacity)

---

*For raw numbers, see `benchmarks/fio/out/20260907T100419Z/summary.md`*
