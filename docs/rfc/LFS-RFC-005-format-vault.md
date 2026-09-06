# LFS-RFC-005: The Format Vault — a 200-Year On-Disk Format

- Status: Adopted (3.6 implements Phases F1–F4)
- Depends on: LFS-RFC-002 (architecture), LFS-RFC-004 (capacity plane)
- Implementation: `src/common/version.rs`, `src/ondisk/conformance.rs`,
  `tools/conformance/`, `tools/upgrade/`, `src/security/kdf.rs`
  (EnvelopeV2), `specifications/format_vault.md` (the 3.6 record)

## 1. The problem

"Future-proof for 200 years" is not a feature; it is a DISCIPLINE
applied to every on-disk byte. Filesystems die three ways: the format
cannot be read by software that no longer exists (obsolescence), the
format cannot express new capabilities without a reformat (rigidity),
and nobody can tell a healthy image from a corrupt one decades later
(verifiability). This RFC commits LionFS to the countermeasures, most
of which 3.6 ships.

## 2. F1 — The capacity horizon (inherited, verified)

The default 128-bit address plane and the opt-in 256-bit `WideAddr`
plane are never the bottleneck (RFC-004's arithmetic: ~10^20 years at
16 GiB/s). Timestamps are i64 seconds (valid to year ~292 billion).
Inode identifiers are u64 with the HAMT namespace as the growth path.
No 3.6 change narrows any horizon. **Policy: every RFC must re-state
the horizons it touches and prove it does not narrow them.**

## 3. F2 — Feature flags, not version bumps (3.6)

The format `version` stays 2. Capabilities are named, sticky bits in
`Superblock::fs_features`:

| Bit | Capability |
|-----|-----------|
| `1<<0` FS_FEATURE_XATTR | xattr/ACL tree (node type 13) |
| `1<<1` FS_FEATURE_REFLINK | clone registry + shared blocks under pinning |
| `1<<2` FS_FEATURE_ENVELOPE_V2 | key envelope v2 at `key_envelope_block` |

Rules:

1. A bit is set ONLY when its structures first appear, and never
   cleared.
2. Every build knows `KNOWN_FS_FEATURES`; a mount of an image with
   any unknown bit is REFUSED -- by the core (`LionFS::new`), not
   merely the CLI.
3. Old builds that ignore unknown bits (3.5 never reads
   `fs_features`) keep mounting and serving what they understand;
   new structures are invisible to them. This is the ZFS posture.
4. New on-disk fields are carved from zeroed padding
   (`spill_extent_root`, `node_generation`, `xattr_tree_root`,
   `key_envelope_block` are the precedents): old builds read them as
   "absent", in both directions.
5. A version bump happens only for a change that cannot be expressed
   as an opt-in structure. None has been needed since v2.

## 4. F3 — Verifiability: self-describing structures + the conformance battery (3.6)

Every 3.6 structure is self-describing -- magic, version, checksum:

* "LXAT" xattr blocks (fletcher32 over the entry region);
* "LFSE" key envelopes (fixed 96-byte record);
* "LFSS" replication streams (per-file SHA-256 + manifest digest);
* the pre-existing checksum tree (per-block, algorithm-id'd) remains
  the data plane's integrity backbone.

The conformance battery (`ondisk::conformance`) is the format's
TESTABLE CONTRACT: 14 read-only checks over superblock integrity,
geometry, tree reachability, checksum spot-verification against
on-disk bytes, registry sanity, bitmap/superblock agreement, and the
journal tail. It is run by the `lfs_conformance` tool, GATES
`lfs_upgrade`, and runs against populated images in the test suite --
so format drift breaks CI, not user data.

**Policy: every new on-disk structure ships with (a) a self-describing
header, (b) a conformance check, (c) a money test that round-trips it
across unmount.**

## 5. F4 — Crypto agility (3.6)

Every algorithm is DATA on disk, dispatched at runtime:
`ChecksumTreeValue.algorithm_id`, `Inode.encryption_algo`,
`KeyTreeRecord.algorithm`, and the new `EnvelopeV2`
(`kdf_id` / `aead_id` / `kem_id`). A volume can migrate to a stronger
digest or cipher without a reformat; mixed-algorithm images verify
block-by-block. The KEM slot is the reserved seam for post-quantum
key wraps: adding ML-KEM is a rewrap, not a reformat. Ids are
appended, never re-meaned; an unknown id is a loud
`UnsupportedAlgorithm`, never a guess.

## 6. F5 — Migration archaeology (shipped in parts)

`lfs_dump` / `lfs_debug` / `lfs_inspect` render raw structures;
`lfs_conformance` states what the format PROMISES; the LFSS stream is
portable across endianness by construction (explicit LE) and carries
its own manifest. **Policy: a reader with the spec and the tools must
be able to reconstruct every byte's meaning on a decades-old image.**

## 7. What 3.6 explicitly does NOT claim

* No hardware-longevity guarantees (media rot is the scrub/heal
  plane's problem; the wired scrubber is the 3.6 answer for parity
  pools).
* No PQ algorithm ships yet -- the KEM slot is format, not code.
* The conformance battery is necessary, not sufficient: it samples
  checksums rather than verifying every block (a full verify is the
  scrubber's job and costs a full read).
* The sweep enumeration is memory-bound (documented); paged iteration
  is future work.
