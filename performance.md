# LionFS Performance Tuning & Architecture

LionFS is designed from the ground up for extreme performance and micro-optimization. 

## Architectural Performance Features

1. **Copy-on-Write (CoW) B-Trees**: Uses highly optimized lock-free or read-optimized B-Tree structures for instant snapshots and minimal locking overhead.
2. **Transaction Batching**: Multiple file operations (like FUSE 128KB chunks) are buffered in memory and batched into a single WAL journal transaction. This avoids excessive syncs and disk thrashing.
3. **Extent-based Allocation**: Files allocate space in extents rather than single blocks. Extents are automatically merged when contiguous blocks are allocated, reducing metadata overhead.
4. **SIMD & Acceleration Ready**: Checksumming (XxHash64) and crypto operations are designed to map to hardware-accelerated instructions where possible.

## Current Bottlenecks and Mitigations

- **FUSE Overhead**: The Linux FUSE subsystem introduces boundary crossing overhead. This is mitigated by batched transactions and large `max_write` buffer sizes.
- **Lock Contention**: Early phases had heavy lock contention during concurrent directory writes. This is mitigated by fine-grained locking on B-Tree nodes and a unified active transaction cache.

## Micro-optimizations in Phase 11

- Implemented `active_tx` buffering in the `LionFS` FUSE daemon to bundle up to 1024 dirty blocks or subsequent operations before flushing to the WAL.
- Eliminated redundant `disk.sync()` calls during WAL commits, relying on periodic `fsync` and kernel page caching for bulk data.
- Optimized `allocate_extents` for contiguous bitmap scanning.
