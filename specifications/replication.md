# Snapshot Replication: send / recv (3.6)

Status: implemented (stretch goal shipped). Money test:
`phase12_tests::send_recv_roundtrip_recreates_the_tree_on_a_fresh_image`.

## Stream format ("LFSS" v1)

All integers little-endian; paths relative, `/`-separated,
self-describing end to end:

```
Header  : magic "LFSS" u32, version u16=1, flags u16=0,
          snapshot_id u64, created u64, barrier u64, pool_uuid 16B
DirRec  : tag u8=2, path_len u16, path, mode u32, uid u32, gid u32,
          mtime i64
FileRec : tag u8=1, path_len u16, path, mode u32, uid u32, gid u32,
          mtime i64, size u64,
          chunks (len u32 + bytes)* terminated by len==0,
          sha256 u8[32]                -- digest of the plaintext bytes
End     : tag u8=0xFF, file_count u32, sha256 u8[32]
                                     -- digest over the per-file digests
```

## send

Walks the SNAPSHOT's frozen view: frozen inode tree
(`SnapshotManager::read_snapshot_inode`), frozen directory listings
(`DirManager::read_entries` against the SNAPSHOT's checksum + bad-block
roots), frozen per-block verification
(`SnapshotManager::read_snapshot_csum` for EVERY block as it is read).
The stream carries exactly what the snapshot froze, bit for bit, or
the send fails. Chunked at 64 KiB; content travels as plaintext so the
receiver re-compresses under its OWN policy (cross-compression
replication for free).

## recv

Replays the stream through the ordinary `VfsOps` write path (the same
surface every client uses -- no special import code), verifies every
file's SHA-256 and the manifest, then freezes the received state as a
snapshot with the SOURCE's id. Crash mid-recv = a partially populated
image with no snapshot: recv again (files are overwritten).

## Honest limits

* No incremental deltas yet (whole-file records). The obvious 3.7
  feature; the header reserves `flags` for a parent-id field.
* No symlink records -- the engine does not store symlinks yet.
* Encrypted inodes are REFUSED at send time (the plaintext lives
  behind key material a cold snapshot walk does not carry).
* No resume: a torn stream fails digest verification at the boundary
  and is re-sent whole.
