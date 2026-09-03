# Specification: Copy-GC & Space Reclamation (LionFS 3.0)

Status: implemented (`src/gc/`) | RFC: LFS-RFC-004 §6

## Why

CoW + snapshots leave stale extents behind until refcounts hit zero;
a filesystem that never reclaims them leaks capacity at the CoW
rate. The 1.x "the GC worker will handle it" comment was a comment.

## Cost model (Rosenblum-Ousterhout, extended)

```text
benefit = freeable × (1 + age/age_half_life)     // cold-data prior
cost    = 2 × live × (1 + wear_bps/1e4)          // + flash wear leveling
score   = benefit / cost                         // highest first
```

Defaults: age half-life 7 days (snapshot/CoW churn is daily-ish),
wear 5 bps per 100 write cycles. A fully-dead segment scores
`u64::MAX/2` (headroom keeps tiebreaks sane); zero-freeable scores
0 (never picked).

## Watermarks and panic mode

| Free space | Mode | Behavior |
|-----------|------|----------|
| ≥ 20% (kick) | Idle | no plan |
| 20%..8% | Background | score-ordered trickle, Bulk QoS class |
| < 8% (aggressive) | Aggressive | pure freeable-bytes ordering — at 8% free, reclaim now, don't be clever |

Plans cap at `max_segments_per_plan` (default 8) with deterministic
tiebreak (segment id). An all-live pool below the watermark returns
`None`: "full of live data" is an operator report, not an infinite
loop.

## Execution contract

Planner output is `GcPlan { urgency, segments, estimated_copy_bytes
(= 2× live), estimated_reclaimed_bytes }`. The transaction layer
executes relocations through the ordinary CoC write path —
checksummed, journaled, crash-recovered like any user write — in
the `Bulk` QoS class. `ReclaimEvent` records (segment, freed bytes,
timestamp) update the census incrementally as refcounts drop: no
device-wide rescans.

## Tunables (`GcConfig`)

- `kick_pct` / `aggressive_pct` (20 / 8)
- `wear_bps_per_100_cycles` (5)
- `age_half_life_ns` (7 days)
- `max_segments_per_plan` (8)

## Kept fixed

- u128 intermediates everywhere (the first draft mixed u64/u32 and
  drowned in type inference).
- Panic-mode ordering degradation is *policy*, not a bug: tested
  side-by-side with background ordering.
- The 2.0 honest-failure discipline: unreclaimable + low free =
  `None` + report, never a spin.
