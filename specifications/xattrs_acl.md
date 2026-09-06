# Extended Attributes & POSIX ACLs (3.6)

Status: implemented and wired. Money tests: `src/fs/phase12_tests.rs`
(xattr roundtrip + remount persistence, flags/namespace rules, ACL
evaluation, chmod sync, mkdir inheritance).

## Design

**Storage.** One `BTree<u64 /*ino*/, XattrRecord>` (node type **13**,
rooted at `Superblock::xattr_tree_root`); the record points at the
inode's 4 KiB "LXAT" block (`ondisk::xattr`). One record per inode WITH
xattrs -- inodes without xattrs cost nothing. This mirrors ext4's
one-xattr-block-per-inode shape but is fully self-describing (magic +
version + fletcher32 + TLV), which the Format Vault requires: a future
reader with only this module can decode every attribute on a
200-year-old image.

**Freezing.** Node type 13 is a frozen tree (`btree::is_frozen_tree`),
so path-copy CoW applies while a snapshot is live -- identical to the
dir-name / checksum trees. `SnapshotRecord.reserved[0]` carries the
frozen xattr-tree root so a snapshot's xattr view is readable.

**Root plumbing (the load-bearing detail).** The B-tree publishes root
moves through `ctx.set_root_cell(13, ..)`; `SharedCore::commit_tx`
syncs that cell into `Superblock::xattr_tree_root` at commit. FIRST
USE is the subtle case: the tree does not exist (root 0), and a
transaction whose B-tree never splits publishes no root cell -- the
pointer would live only in memory and die in a crash. First-use init
therefore calls `ctx.set_root_cell` itself, which also forces the
commit to persist all superblock slots. (Found the hard way: the
money test's destroy+remount lost the first xattr before the fix.)

**Namespaces.** `user.` / `security.` / `trusted.` / `system.` with
the Linux rules: user.* is refused on symlinks; capacity is one block
(4064 bytes of entry region) -> ENOSPC; not-found is ENODATA per
xattr(7); XATTR_CREATE / XATTR_REPLACE semantics.

## ACLs

`security::posix_acl` implements POSIX 1003.1e draft 17 in the
ext4-compatible wire format (version word + 8-byte
`{tag, perm, id}` entries, canonical order). Implemented semantics:

* structural validation (base classes present, mask required iff
  named entries exist, no duplicate ids, rwx-only permission words,
  default ACLs need a named entry);
* `mode_bits` -- the stat(2) derivation (group class is GROUP_OBJ
  masked by MASK when named entries exist);
* `evaluate` -- the draft-17 access algorithm (owner / named user /
  owning+named group vs MASK / other, with the no-OTHER-fallback
  rule once a group class matched);
* `apply_chmod` -- chmod(2) remaps USER_OBJ / MASK-or-GROUP_OBJ /
  OTHER and keeps named entries;
* `inherit_access_from_default` -- mkdir(2) intersection with the
  create mode, mask recomputed.

Wiring: `setxattr(system.posix_acl_access)` validates and re-derives
the inode's mode bits in the SAME transaction; `setattr` with a mode
change re-derives the ACL (chmod sync); `mkdir` inherits the parent's
default ACL in the same transaction as the child's inode write;
`access()` evaluates the ACL when one is stored, else the mode bits.

## Honest limits

* The VFS `access()` surface carries the caller's uid and PRIMARY gid
  only -- no supplementary-group list. With the FUSE
  `default_permissions` mount option the kernel's full credential set
  applies; without it, LionFS's own evaluation is the authority.
* One 4 KiB xattr block per inode (the ext4 shape); no overflow
  chain yet (the LXAT header reserves `next_block` for it).
* ACL xattrs cannot be removed directly (EACCES) -- clear them with a
  trivial ACL / chmod, matching the kernel's own rule.
