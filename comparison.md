# LionFS vs Other Filesystems

A comparison of LionFS against standard Linux filesystems.

## Feature Comparison

| Feature | LionFS | Btrfs | ZFS | ext4 | XFS |
|---------|--------|-------|-----|------|-----|
| Copy-on-Write (CoW) | Yes | Yes | Yes | No | No (reflink only) |
| Checksumming | Yes | Yes | Yes | Metadata | Metadata |
| Snapshots | Yes | Yes | Yes | No | No |
| Deduplication | Planned | Offline | Inline | No | No |
| Architecture | Rust (Memory Safe) | C | C | C | C |
| Built-in RAID | Planned | Yes | Yes | No | No |

## Performance Profile

**LionFS vs ext4**
- **Pros**: Better metadata safety, instant snapshots, data integrity validation.
- **Cons**: CoW fragmentation overhead for random writes (similar to Btrfs/ZFS).

**LionFS vs Btrfs**
- **Pros**: Written in Rust, memory safety eliminates whole classes of filesystem corruption bugs. Streamlined B-Tree implementation.
- **Cons**: Less mature ecosystem, fewer advanced features like send/receive (currently in development).

**LionFS vs ZFS**
- **Pros**: Lighter on memory (ZFS ARC is memory intensive), GPL/Linux compatible natively without DKMS issues.
- **Cons**: ZFS has decades of enterprise hardening and advanced caching algorithms (L2ARC, ZIL).
