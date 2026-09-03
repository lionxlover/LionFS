# Specification: Capacity Plane — 256-bit Dynamic Addressing (LionFS 3.0)

Status: implemented (`src/addressing/va256.rs`) | RFC: LFS-RFC-004 §3

## The selector

`CapacityPlane` is a mkfs-time choice recorded in the superblock's
`plane` byte. Mount refuses a plane it does not understand — the
forward-compatibility gate. `Compact` (128-bit `VolumeAddr`) is the
default and covers every volume a single host owns; `Wide`
(256-bit `WideAddr`) is opt-in, for fabric pools.

## WideAddr layout (big end first)

| Bits | Field | Meaning |
|------|-------|---------|
| 255-232 | `domain_id` (24) | management / trust domain |
| 231-208 | `namespace_id` (24) | tenant / subvolume within domain |
| 207-176 | `volume_id` (32) | container / replicated set |
| 175-144 | `region` (32) | stripe, band, or zone-set |
| 143-112 | `device` (32) | pool member (4.29 G devices max) |
| 111-64 | `device_lba` (48) | per-device address, 4 KiB units (1 EiB/device) |
| 63-0 | `byte_offset` (64) | byte granularity within the block |

The trailing byte offset makes byte-addressable tiers (PMEM, CXL)
first-class: one comparison ordering names a byte of PMEM, a block of
NVMe, and a sector of SMR. `Ord` is field order (device-local runs
sort together), matching `VolumeAddr`'s discipline.

## The embedding

A compact address is a *prefix* of its wide image: `From<VolumeAddr>`
is total, and `try_compact` returns `Some` exactly when every
wide-only field is zero and `byte_offset == 0`. Round-trips are
property-tested (`compact_embedding_is_lossless`).

```rust
let c = VolumeAddr::compose(9, 100, 200, 1 << 40)?;
let w: WideAddr = c.into();          // wide-only fields zero
assert_eq!(w.try_compact(), Some(c)); // lossless
```

## Field validation

`WideAddr::compose` validates field widths (`None` on overflow — the
mkfs/mount path); `compose_unchecked` carries debug assertions for
hot paths. `advance_blocks` stays on-device and refuses byte
addresses; `same_device` is the allocator's locality test.

## Cost accounting

`WideAddr` is `[u64; 4]`. Hash: 4 multiplies. Compare: ≤ 4
subtractions. Measured on the 3.0 bench set: +4% insert, +2% lookup
vs `Extent16`/`VolumeAddr`. Wide-plane extents are 32-byte records
with the same GRAN/RAW/ENC/SHARED/DEDUP flags discipline.

## Capacity math (the honest table)

- Compact: 2^128 addresses × 4 KiB = 2^140 B ≈ 2^112 YiB.
- Wide: 2^268 B addressable. Devices: 4.29 G × 1 EiB = 4.29 G EiB
  per (domain, namespace, volume, region) tuple; 16.7 M namespaces
  per domain; 16.7 M domains.
- "Unlimited" means: beyond any forecastable storage growth for the
  machine's service lifetime. The number is above; the box-art
  wording is cheaper.

## Kept fixed

- Field-order `Ord` (not numeric limb order) — verified equivalent
  for this layout, kept explicit for future layout changes.
- `plane` tag byte stability: Compact=0, Wide=1; unknown tags refuse
  to mount (tested).
- The 16-step log2/entropy quantization tables are NOT used here
  (pure integer field math only).
