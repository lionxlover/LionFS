# The Format Vault (3.6)

Status: implemented and wired. Normative program:
`docs/rfc/LFS-RFC-005-format-vault.md`.

## Components

### 1. The feature-flag protocol

The format `version` stays 2; capabilities are named bits in
`Superblock::fs_features` (`common::version`):

* `FS_FEATURE_XATTR` -- the xattr/ACL tree exists;
* `FS_FEATURE_REFLINK` -- the clone registry exists, blocks may be
  shared under pinning;
* `FS_FEATURE_ENVELOPE_V2` -- the key envelope v2 blob exists.

A bit is set only when the corresponding structures first appear, and
is sticky. Old builds ignore the bits (they never read
`fs_features`), so a 3.5 build keeps mounting a 3.6 image and serving
the data it understands; the new structures are invisible to it. New
builds REFUSE images with unknown bits (`is_mountable`, enforced in
`LionFS::new` -- the core, not just the CLI).

### 2. The conformance battery

`ondisk::conformance::run(disk, sb, verify_blocks)` -- read-only,
three consumers (the `lfs_conformance` tool, the `lfs_upgrade` gate,
and the lib test suite so format drift breaks CI, not user data):

1. `superblock_magic`, 2. `format_version`, 3. `feature_flags`,
4. `superblock_checksum`, 5. `geometry_sanity`,
6. `superblock_slots_agree` (valid slots agree on the live roots),
7. `bitmap_free_count` (superblock vs bitmap -- caught a REAL
   pre-existing drift: mkfs never subtracted the reserved slots),
8. `inode_tree` (validate + structural record check),
9. `checksum_tree` + 10. `checksum_spot_verify` (deterministic
   stride sample re-reads blocks and verifies on-disk bytes),
11. `snapshot_records`, 12. `clone_registry`, 13. `xattr_blocks`
   (every LXAT block parses), 14. `journal_tail`.

### 3. The offline upgrade tool

`lfs_upgrade <image>`: (1) run the battery, REFUSE on any failure;
(2) commit this build's feature registry + stamp high-water to all
superblock slots. Idempotent, offline-only, never invents structures
that are not already on disk. The 3.5 binary printed ok and exited.

### 4. Live accounting (the battery's first catch)

`sb.free_blocks` had been static since mkfs. 3.6:
`Transaction.alloc_delta` (net blocks) folds into the superblock at
every commit; clean unmount checkpoints the superblock; mkfs and the
fixtures subtract the reserved secondary slots. statfs free space is
now a real number, and the battery's bitmap check is exact.

## Version policy (short form)

* v2 stays; features are bits. A version bump happens only for a
  change that cannot be expressed as an opt-in structure (none so
  far since v2's clusters).
* New on-disk fields are carved from zeroed padding (the
  `spill_extent_root` / `node_generation` / `xattr_tree_root` /
  `key_envelope_block` precedents) so old builds read them as
  "absent".
* Every new structure is self-describing (magic + version +
  checksum) -- LXAT blocks, LFSE envelopes, LFSS streams.
* Every algorithm id is data on disk, dispatched at runtime --
  crypto agility without reformat.
