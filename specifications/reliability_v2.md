# Specification: Reliability — Recovery, Checksums, Healing, Erasure (LionFS 2.0, Pillar III)

Status: implemented (`src/recovery/state_machine.rs`, `src/integrity/dual_speed.rs`,
`src/integrity/healer.rs`, `src/pool/erasure.rs`) | RFC: LFS-RFC-002 §5

## The five-state recovery machine

Mount recovery is a state machine with one obligation per transition:
make the smallest change that restores a provably consistent view,
then get out of the way.

1. **PROBE** — read SB0/SB1/SB2; choose the highest-generation
   CRC-valid copy; no valid copy fails the mount explicitly.
2. **REPLAY** — walk the intent log; roll forward committed
   transactions, discard open ones (safe because redirect-on-write
   leaves their partial writes in blocks no live tree references).
3. **CHECKPOINT** — swap roots under generation+1; reset the journal.
4. **RECONCILE** — merge bad-blocks and ZNS zone tables with device
   reports (the device is authoritative).
5. **WRITABLE** — open rings, start shards, accept submissions.

The machine is generic over a `RecoveryBackend` (the real one wraps
`Disk` + journal replay; tests drive fakes that fault at named
kill-points). Tested: full-order walks, highest-valid-generation
selection, corrupt-superblock refusal, granular stepping, and the
§9.5 property — a fault at any boundary, re-run converges. The audit
trail (`TransitionRecord`) renders human-readable for the health bus.

## Dual-speed checksums (`integrity::dual_speed`)

Integrity verification split by temperature (strength and speed trade
linearly):

| Class | Algorithm | Digest | Applied to |
|---|---|---|---|
| `HotPage` | xxHash64 | 8 B | hot 4-64 KiB pages, verified on read completion |
| `ColdPage` | `BLAKE3-128` | 16 B | cold pages (0.39% of a 4 KiB block — the 0.4% budget) |
| `CompressionCluster` | BLAKE3-128 (domain-separated) | 16 B | every cluster: corruption detected *before* codec decode failure |
| `Structural` | CRC32C | 4 B | commit records, superblocks (torn-write detection) |

`ClusterTag::compute(inode, cluster, payload)` folds the inode and
cluster index into a domain-separation prefix — a cluster's tag cannot
be confused with another's (the "same bytes, different cluster"
attack on content-only tags). Verification is constant-time for the
cryptographic classes. A class mismatch between stored and requested
digest is flagged (`None`), not silently passed.

## Autonomous healing (`integrity::healer`)

Detection without repair is surveillance. The healer plans:

```text
Quarantine (bad-blocks tree) → AllocateFresh → Reconstruct →
WriteWithFreshChecksum → SwapExtentInTransaction → ReleaseOldExtent
```

- `choose_repair_source` maps the pool profile + degraded set to the
  reconstruction source: `ParityP` (RAID5), `ParityPQ` (RAID6 with the
  second erasure), `Mirror` (RAID1/10), or `NoRedundancy`.
- **No redundancy is an honest failure**: the plan becomes
  `Quarantine → ReportLoss` — the loss event goes to the health bus,
  never a pretend repair.
- The swap step precedes the release step *inside a first-class
  transaction*: a crash mid-repair heals into old or new, never
  neither (tested by construction: swap < release in the step order).

## Generalized Reed-Solomon (`pool::erasure`)

RS(n, k) over GF(2^8) using the 1.x gf256 tables — beyond RAID5/6 to
any k-of-n (e.g., RS(10,6): four concurrent device losses at 60%
storage efficiency):

- **Encoding matrix**: the n×k Vandermonde with *per-row* distinct
  evaluation points (row r = [1, x_r, x_r², …, x_r^(k-1)], x_r = r+1),
  right-multiplied by the inverse of the top k×k block
  (`M = V·V_k⁻¹`). Right-multiplication by an invertible matrix
  preserves the any-k-rows independence (MDS property) — the
  construction used by Jerasure/Backblaze-class libraries. (A naive
  row-reduction of the transposed layout silently destroys the parity
  rows; this was found and fixed during implementation, documented
  here so it stays fixed.)
- Data shards pass through (systematic top); parity shards are
  table-driven XOR folds.
- Reconstruction solves the k×k GF(256) system over the surviving
  shards by Gauss-Jordan, then re-encodes.
- Correctness by property test: 200 rounds of *random* erasure sets
  (any k of n recover), plus single/double erasure, parity-only-plus-
  data reconstruction, zeros, and the RAID6-class single-loss
  equivalence.

| Code | Tolerates | Efficiency |
|---|---|---|
| RS(4,2) | 2 | 50% |
| RS(6,4) | 2 | 67% |
| RS(8,5) | 3 | 62.5% |
| RS(10,6) | 4 | 60% |

GF(256) caps n at 255 shards (asserted); wider pools need
bytes-per-word growth, which is explicitly out of scope and stated.
