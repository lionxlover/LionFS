# Crypto & Format Agility (3.6)

Status: implemented and wired. See also
`docs/rfc/LFS-RFC-005-format-vault.md` (the normative agility
registry) and `specifications/format_vault.md`.

## Principle

A 200-year format cannot hardcode algorithms; it must carry an
ALGORITHM REGISTRY on disk and dispatch at runtime. LionFS 3.6
completes that for every crypto-adjacent primitive:

| Primitive | Where the id lives | Dispatch |
|---|---|---|
| Per-block checksum | `ChecksumTreeValue.algorithm_id` (since 3.2) | `ChecksumAlgorithm::from_u8` on every read |
| Write-path checksum | POLICY: `LFS_CSUM` env (xxh64 default, crc32c, sha256, blake3) | `algorithms::write_path_algorithm()` |
| Per-inode cipher | `Inode.encryption_algo` (since 1.x) | `EncryptionManager::get_algorithm` |
| Per-key cipher | `KeyTreeRecord.algorithm` (since Phase 7) | same registry |
| Volume key KDF/AEAD | `EnvelopeV2.kdf_id` / `.aead_id` (NEW) | `EnvelopeV2::unwrap` |
| Post-quantum KEM | `EnvelopeV2.kem_id` (NEW, reserved slot) | future ML-KEM hybrid |

Because every record carries its own id, images verify
BLOCK-BY-BLOCK under mixed algorithms: repointing a volume at a
stronger digest (or a new cipher) needs no reformat -- new writes use
the new id, old records keep verifying under theirs.

## Envelope v2

The 3.5 `WrappedEnvelope` had NO on-disk home (the struct existed,
nothing persisted it). 3.6:

* `EnvelopeV2` -- 96-byte self-describing record, magic "LFSE",
  version 2, `kdf_id` (1 = PBKDF2-HMAC-SHA256), `aead_id` (the
  ENCRYPTION_* registry: 1 = AES-256-GCM, 2 = ChaCha20-Poly1305),
  `kem_id` (0 = none; the reserved post-quantum slot), iterations,
  salt, nonce, wrapped master (32 + 16 tag).
* On disk at `Superblock::key_envelope_block` (carved from
  `padding2`; old images read 0 = no envelope).
* `mkfs --passphrase` (or `LFS_PASSPHRASE`) writes it and stamps
  `FS_FEATURE_ENVELOPE_V2`; the mount CLI requires a successful
  unwrap before mounting.
* v1 in-memory envelopes remain unwrappable (upgrade path: rewrap
  writes v2).

## Negotiation (the mount gate)

`common::version::is_mountable(version, fs_features)` -- safe iff the
format version is understood AND no unknown feature bits are set.
3.6 wires this into `LionFS::new` ITSELF (previously only the mount
CLI checked the version; a library consumer or tool would mount a
future image and misinterpret fields). The CLI keeps a clearer
message. The known-bit mask for 3.6: XATTR | REFLINK | ENVELOPE_V2.

## Honest limits

* The KEM slot is FORMAT, not code: no PQ algorithm ships yet. The
  slot exists so adding one is a rewrap, not a reformat.
* `LFS_CSUM` is per-mount policy, not persisted per-volume; the
  per-record ids make that safe (mixed algorithms verify) and a
  persisted default is a future superblock-default field.
