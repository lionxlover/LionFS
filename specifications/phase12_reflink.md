# Reflink Clones (3.6)

Status: implemented and wired. Money tests: `src/fs/phase12_tests.rs`
(share + redirect + durability, registry + feature flag persistence).

## Design

A clone maps the SOURCE's physical blocks into the DESTINATION and
pins them via the refcount coverage tree (`RefCountManager::pin_range`)
-- the exact machinery deduplication already uses, and the write path
ALREADY handles the safety rule: a write to a pinned block allocates a
fresh block, copies the old content, and remaps (redirect-on-write).
Two files may share every physical block while both remain writable;
the pin is what makes that safe. Zero data is copied; the clone costs
O(extents) metadata.

Contract (enforced by `reflink_file_in_ctx`):

* source: uncompressed regular file (compressed inodes are refused
  with ENOTSUP -- the cluster path has no per-block identity to share;
  the honest limitation);
* destination: uncompressed regular file of size 0;
* the destination gets its OWN spill extent tree populated with the
  same mappings -- the source's B-tree nodes are never shared (they
  mutate in place when no snapshot barrier is live);
* every shared physical range is pinned BEFORE the mapping becomes
  visible (one transaction; the pin and the mapping commit together);
* a `CloneRecord { id: dst_ino, source_id, generation, shared_blocks }`
  lands in the clone registry (node type 9); the registry root syncs
  through the transaction root cells like every other tree, with
  first-use init publishing its own root cell;
* `FS_FEATURE_REFLINK` is stamped on first use.

Space accounting: shared blocks are NOT re-allocated; `statfs` free
space does not move (Btrfs/APFS behavior -- and, since the Phase 12
live-accounting fix, statfs is a real number). Deleting either side
leaves blocks pinned (the dedup posture); the Copy-GC sweep reclaims.

## Surface

* `VfsOps::copy_file_range` (and the FUSE callback): whole-file,
  zero-offset, empty-destination requests take the reflink path;
  anything else is an honest byte copy through the ordinary
  read/write path (kernel `copy_file_range` semantics).
* `lfs_clone reflink <image> <src> <dst>` -- in-process mount, real
  engine; `lfs_clone list` dumps the registry; `check` is the
  dry-run.

## Honest limits

* Compressed (cluster) inodes cannot be cloned yet.
* Partial-range cloning (`clone_range`) is not exposed; the pin
  machinery is range-based, so it is an incremental extension.
* No FICLONE ioctl plumbing yet -- the FUSE side surfaces
  `copy_file_range` (which `cp --reflink=auto` uses).
