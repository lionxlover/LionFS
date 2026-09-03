# LionFS 1.0 Administration Guide

LionFS provides exactly **25 standalone administrative binaries** for granular management, telemetry, and repair of your volumes.

All tools support standard POSIX CLI flags and output strictly deterministic JSON formatting natively, making them perfect for CI/CD automation pipelines, Grafana ingestion, and Ansible orchestration.

## 1. Core Lifecycle
- `mkfs_lfs`: Creates and formats a new LionFS volume.
- `mount_lfs`: Mounts a LionFS volume into the userspace (FUSE).
- `lfs_admin`: Central administration interface for global flags.
- `lfs_volume`: Modifies volume labels and resizes existing filesystems.

## 2. Integrity & Repair
- `lfs_scrub`: Launches a background self-healing scrubber to read all blocks, verify CRC32/BLAKE3 checksums, and rewrite corrupted data from RAID parity mirrors.
- `lfs_verify`: Verifies B+Tree structural hierarchies and detects orphan inodes.
- `lfs_repair`: Attempts offline correction of catastrophic Superblock or Journal metadata failure.
- `lfs_health`: Emits a top-level JSON health report of the active storage pool.

## 3. Storage Pools & RAID
- `lfs_pool`: Manages storage pools, adds/removes physical drives to the active pool.
- `lfs_raid`: Configures or modifies the active RAID profile (e.g., Single to Mirror).
- `lfs_rebuild`: Triggers an active rebuild array sequence when replacing a failed drive.

## 4. Snapshots & Clones
- `lfs_snapshot`: Creates an instantaneous Copy-on-Write (CoW) read-only snapshot.
- `lfs_clone`: Promotes a snapshot into an independent writable clone.

## 5. Security & Optimization
- `lfs_compress`: Triggers background Zstd/LZ4 compression across uncompressed extents.
- `lfs_dedupe`: Triggers an offline or background block-level deduplication scan.
- `lfs_encrypt`: Re-keys or activates AES-GCM encryption on directories.
- `lfs_keys`: Manages the local cryptographic key hashes stored in the Superblock.

## 6. Telemetry & AI
- `lfs_telemetry`: Dumps real-time IOPS, cache-hit rates, and latency curves.
- `lfs_predict`: Invokes the AI optimization engine to analyze the telemetry database and output predictive caching models for the next 24 hours of operation.
- `lfs_recommend`: Outputs automated tuning advice based on current workloads (e.g., "Increase cache size due to 90% thrash rate").
- `lfs_scheduler`: Modifies background work priority queues (e.g., throttling scrubber during peak business hours).
- `lfs_policy`: Configures global automation policies.

## 7. Development & Benchmarking
- `lfs_debug`: Outputs internal hex-dumps of targeted B+Tree logical blocks.
- `lfs_dump`: Dumps the Superblock and WAL journal for crash investigations.
- `lfs_profile`: Attaches to the userspace process to extract performance flame graphs.
- `lfs_benchmark`: A built-in Criterion/FIO-style benchmarking utility to validate IOPS scalability directly against the storage medium.
