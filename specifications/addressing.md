# Specification: 128-bit Addressing & Packed Extents (LionFS 2.0, Pillar II)

Status: implemented (`src/addressing/`) | RFC: LFS-RFC-002 §4.1

## VolumeAddr — the 128-bit volume address

Every byte LionFS manages is named by a 128-bit address, fixed at mkfs
time and validated at mount:

| Bits | Field | Meaning |
|---|---|---|
| 127-112 | `volume_id` (16) | subvolume / container selector |
| 111-88 | `region` (24) | stripe or band within the pool |
| 87-64 | `device` (24) | pool member (16.7 M devices max) |
| 63-0 | `device_lba` (64) | per-device block address, 4 KiB units |

A 128-bit namespace in 4 KiB units addresses 2^140 bytes. Ordering is
**structured** (volume, region, device, lba) — not numeric — so
device-local runs sort together. Composition is width-checked
(`compose` returns `None` on overflow); `advance_blocks` uses checked
arithmetic; `same_stripe` tests run coalescability.

The 256-bit alternative was analyzed and rejected (RFC-002 §10): no
shipping medium approaches 2^40 blocks while wider keys measurably
split cache lines and slow hashing.

## Extent16 — the packed extent record

Extents are the most numerous structure in the filesystem; their
on-disk width is minimized to 16 bytes (one cache line holds eight):

```text
byte 0..6   logical_start  : u48
byte 6..12  physical_start : u48
byte 12..15 length         : u24
byte 15     flags          : u8
```

Flags (`ExtentFlags`, a plain u8 newtype — no bitflags dependency):

| Bit | Name | Meaning |
|---|---|---|
| 0 | `GRAN` | 0 = fields count 4 KiB units (file max 1 EiB); 1 = 64 KiB units (16 EiB) |
| 1 | `RAW` | stored uncompressed |
| 2 | `ENC` | payload encrypted |
| 3 | `SHARED` | refcounted (snapshot/dedup) |
| 4 | `DEDUP` | dedup-layer reference target |
| 5-7 | reserved | must be zero on decode (forward-format detection) |

Semantics:

- `encode` rejects out-of-width fields (`None`, never truncation);
  `decode` rejects reserved flag bits.
- `intersects_logical` / `map_logical_to_physical` are the read path's
  extent probe.
- `coalescable_with` implements the B-epsilon flusher's merge test
  (logical adjacency + physical adjacency + flag identity).
- `logical_end` / `length_bytes` are **saturating**: a maximally packed
  GRAN=1 record reaches the u64 edge and reports `u64::MAX` rather
  than wrapping into a lie.
- `bytemuck` `Pod`/`Zeroable` — direct on-disk casting.

## Interop with the 1.x format

`LBA_BLOCK_BYTES` (4 KiB) matches the 1.x `BLOCK_SIZE`, so the two
address spaces interoperate; the v2→v3 upgrade path maps 64-bit block
numbers into `device_lba` with `volume_id`/`region`/`device` zeroed.
