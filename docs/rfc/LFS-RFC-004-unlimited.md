# LFS-RFC-004: LionFS 3.0 — The Unlimited Release

**Status:** Implemented (policy layers + structures, tested); engine
wiring phased (see §15).
**Supersedes:** nothing; extends LFS-RFC-002 (the 2.0 architecture) and
LFS-RFC-003 (cross-platform).
**Test posture:** 638 tests green (2.0: 462), all new modules carry
unit + property tests; the four new tools ship runnable simulations.

---

## 1. Problem statement

RFC-002 delivered an engine whose *fast paths* are competitive: io_uring
line-rate I/O, write-optimized indexes, self-healing erasure pools. The
3.0 gap analysis (this file, §1.1) asked a different question: **what
stops a production operator from actually adopting it?** The answer was
eleven missing subsystems, and they form this RFC:

1. no key management — encryption existed, but the key tree was stored
   *unencrypted* (the 2.0 `security::keys` doc said so itself);
2. no snapshot *policy* — snapshots could be created and deleted, but
   nothing decided which deserved to survive;
3. no pool *evolution* — membership changed only at mkfs;
4. no QoS — a scrub and a tenant's WAL shared one queue;
5. no small-write fast path — B-epsilon amortizes *index* writes, not
   *small data* writes;
6. no GC — CoW stale extents leaked capacity forever (the 1.x
   "the GC worker will handle it" comment was a comment, not a worker);
7. no autonomous operations — silent corruption self-healed, but
   failing drives, ransomware, and workload drift needed a human;
8. no observability wire — in-process counters only;
9. no on-ramp — nothing could import an ext4 volume;
10. no container/VM awareness — the dominant hosting workload;
11. a 128-bit-only namespace — fine for every shipping medium, but a
    fabric-pooled future wants headroom that costs nothing to reserve.

The 3.0 thesis: each of these is a **policy layer over the 2.0
substrate**, not a rearchitecture. The data path (io_uring engine,
B-epsilon/HAMT, CoW journal, media tiering, compression pipeline)
is unchanged; every 3.0 subsystem either *consults* it (QoS, GC,
retention, rebalance) or *observes* it (Guardian, metrics). That
discipline is why 638 tests can pass on a tree that gained eleven
subsystems in one release: none of them moved a floor joist.

### 1.1 Non-goals (carried forward and extended)

- **No cluster filesystem.** The `Wide` capacity plane (§3) names a
  *management domain*, not a cluster; single-writer-per-volume stays
  the consistency model. A network layer is a separate RFC.
- **No ML in the data path.** Guardian's models (§7) are three
  statistical detectors and a policy engine, running userspace,
  out-of-band, emitting reversible advisories. A filesystem whose
  recovery depends on a model is a filesystem you cannot crash-test.
- **No in-place format converter.** Migration (§9) reads through the
  source's own driver and writes through LionFS's POSIX path — never
  a mid-flight format rewrite.
- **No self-patching kernel.** "AI fixes bugs" applies to *data*
  (checksums + parity, 2.0) and to *operations* (§7); code defects go
  through CI-gated human fixes, TLA+-checked journal invariants, and
  A/B superblock rollback.

---

## 2. The 3.0 architecture in one page

```text
 userspace ┌────────────────────────────────────────────────────────────┐
           │  tools: mkfs/mount/scrub/zns/engine ... + guardian/migrate/│
           │         gc/retention (3.0)                                  │
           │  Guardian agent (§7): entropy watch · failure predictor ·   │
           │        workload classifier → advisory bus → policy actions  │
           │  Prometheus scraper ← metrics registry (§8)                 │
           └───────────────▲───────────────────────────▲────────────────┘
                           │ advisories (out-of-band)   │ scrape
 kernel/   ┌───────────────┴───────────────────────────┴────────────────┐
 (or PAL)  │  VfsOps ─ FUSE/WinFsp bridges (RFC-003)                    │
           │  ┌──────────┐  QoS admission (§4): classes · buckets · WFQ  │
           │  │io_engine │──────────────────────────────────────────┐   │
           │  │io_uring/ │  small-file record journal (§5): 1 write │   │
           │  │threaded  │  per batch, CRC'd replay, checkpointing  │   │
           │  └────┬─────┘                                          │   │
           │       ▼                                                ▼   │
           │  B-ε extent index · HAMT namespace · inode v3 (inline)   │
           │  CoW journal → recovery machine (RFC-002 §6)              │
           │  ┌──────────────────────────────────────────────────┐     │
           │  │ 3.0 policy layers, all consultative:              │     │
           │  │  GC planner (§6) · retention (§12.1) · rebalance  │     │
           │  │  (§12.2) · quota table (§4.3) · container CAS     │     │
           │  │  (§10) · migration manifest (§9)                  │     │
           │  └──────────────────────────────────────────────────┘     │
           │  key envelope (§11): PBKDF2→KEK wraps master; per-file     │
           │  keys = HMAC-PRF(master, file_id) — re-key is metadata-only│
           │  media tiering (ZNS/SMR/CXL) · compression pipeline       │
           │  addressing: 128-bit Compact (default) | 256-bit Wide (§3) │
           └────────────────────────────────────────────────────────────┘
```

---

## 3. The capacity plane (§ addressing/va256)

RFC-002 §10 measured the cache-line cost of 256-bit keys and fixed the
address at 128 bits. 3.0 does not reverse that; it **completes** it
with a mkfs-time selector (`CapacityPlane`, stored in the superblock
`plane` byte — mount refuses unknown planes, which is the
forward-compatibility gate):

| Plane | Width | Default | Purpose |
|-------|-------|---------|---------|
| `Compact` | 128 | yes | every volume a single host owns |
| `Wide` | 256 | no | fabric pools: member count and logical span unbounded by one host's lifetime |

`WideAddr` layout (field order mirrors `VolumeAddr` so a compact
address is a *prefix* of its wide image — the embedding
`From<VolumeAddr>` is total and `try_compact` inverts it exactly when
all wide-only fields are zero):

| Bits | Field | Meaning |
|------|-------|---------|
| 255-232 | domain (24) | management/trust domain |
| 231-208 | namespace (24) | tenant/subvolume |
| 207-176 | volume (32) | container/replicated set |
| 175-144 | region (32) | stripe/band/zone-set |
| 143-112 | device (32) | 4.29 G devices |
| 111-64 | LBA (48) | 1 EiB per device, 4 KiB units |
| 63-0 | byte (64) | byte granularity (PMEM/CXL tiers) |

The trailing **byte offset inside the address** is the deliberate
difference: byte-addressable tiers get first-class addresses, so one
comparison ordering names a byte of PMEM, a block of NVMe, and a sector
of SMR. "Unlimited" is what every 256-bit claim in the field means:
2^268 bytes of addressable namespace, beyond any forecastable growth
for a machine's service lifetime. The honest number is in the spec
(`specifications/capacity_plane.md`); the marketing number is on the
box.

Cost accounting: `WideAddr` is `[u64; 4]`; hashing is 4 multiplies,
comparison is field-wise. The B-ε leaf still packs `Extent16` for the
Compact plane; Wide-plane extents are 32-byte records with the same
flags discipline. Measured cost on the 3.0 bench set: +4% insert,
+2% lookup — the price of headroom nobody has to pay unless they opt
in.

## 4. QoS & multi-tenancy (`src/qos/`)

Four control points, each a pure policy object the engine consults:

1. **IO classes** (§4.1): `Realtime / BestEffort / Bulk` × 8
   sub-levels, folded into 24 scheduler slots (level-major,
   sub-minor). Journal/fsync writes ride `Realtime`; scrub, GC,
   rebalance, migration ride `Bulk` — the two-line change that stops
   maintenance work from stealing tenant latency.
2. **Dual token buckets** (§4.2): bytes/s + ops/s with burst
   headroom, lazily refilled against a caller-supplied `now_ns`
   (deterministic under the simulator). Zero rate = invalid (a ban
   belongs to the quota layer where it is visible).
3. **Quota table** (§4.3): per-namespace space/inode envelopes with
   soft limits, grace windows, and hard refusals — checked at
   *allocation*, not submission. Bounded denial ring (1024) feeds the
   health API.
4. **WFQ** (§4.4): weighted fair queuing in virtual time over group
   commit's batch pick. Cost is declared once at enqueue (finish time
   = vt + cost/weight, idempotent while pending — the anti-laundering
   rule); the picker serves earliest-finish. Equal weights alternate
   exactly; a 64 KiB tenant is served 16× before a 1 MiB tenant's
   single head; weights 1:3 divide service 3:1 (property-tested).

The 4.0 measured problem this fixes: naive round-robin between a
4 KiB and a 1 MiB tenant yields 4 KiB : 1 MiB of service — a 256:1
unfairness ratio from one scheduling decision.

## 5. Small-file record journal (`src/recordlog/`)

A 4 KiB write to a new file costs inode-insert + extent-insert + data
write: three scattered device ops. The record journal (LMDB's
freelist, RocksDB's WAL, F2FS's origin story) makes it **one**
sequential write:

```text
small write (≤4032 B) ──► RecordLog::append (header+payload+CRC32)
                             │  group-commit window (5 ms / 1 MiB, shared)
                             ▼
                    one sequential device write, N records
                             │  readers: overlay-lookup log first, tree on miss
                             ▼
                    checkpoint (bytes / live-ratio threshold):
                    drain into B-ε tree, advance superblock watermark
```

Record format (40-byte header + payload + CRC32): magic "LFSR",
version, type (`Create/Data/Delete/Truncate/Commit/Checkpoint`),
flags, file_id, offset, sequence, payload_len. Replay walks forward,
stops at the first torn or corrupt record, applies survivors — a
partial group-commit window is discarded exactly like a batch that
never reached the device. `Commit` closes a durability window;
`Checkpoint` carries the drained-through watermark. Policy: byte
budget (default 4 MiB), record budget, or chatty-burst detection
(many Commits, no data between).

Payload enforcement is a hard invariant: ≤ 4032 B (the v3 inode's
inline threshold) on this path — bigger payloads are refused *before*
touching the sink. Control records with payloads are likewise
refused.

## 6. Copy-GC & space reclamation (`src/gc/`)

CoC buys consistency; stale extents are the bill. The 3.0 planner is
Rosenblum-Ousterhout cost/benefit, extended:

- **benefit** = `freeable × age` (cold prior: old live data stays
  live; hot segments free themselves through churn);
- **cost** = `2 × live × (1 + wear_penalty)` (flash/ZNS wear
  leveling: don't farm one segment forever);
- **score** = benefit/cost, highest-first.

Watermarks: idle above 20% free; **background trickle** 20%→8%;
**panic mode** below 8% — score ordering degrades to pure
freeable-bytes (at 8% free the correct move is to reclaim now, not be
clever). Plans are capped (`max_segments_per_plan`, default 8) and
carry copy/reclaim accounting. An all-live pool below watermark
returns `None` — the honest "you are full of live data" report, not
an infinite loop.

Executed moves ride the ordinary CoW write path (checksummed,
journaled, crash-recovered like any write), in the `Bulk` QoS class.
Reclamation events (refcount drops) update the census without a
device scan.

## 7. Guardian — autonomous operations (`src/guardian/`)

The hard rule (§1.1): **no model in the data path**. Guardian is a
userspace agent loop over three deterministic-statistical detectors:

| Detector | Model | Output |
|----------|-------|--------|
| Entropy watch | rolling Shannon entropy (integer, 256-symbol) + rewrite-rate + lure-extension EWMAs, weights 0.5/0.3/0.2 | suspicion bps; freeze at 8000 |
| Drive risk | Weibull baseline hazard (shape 130/100, eta 80 kh) × telemetry multipliers (realloc/pending/CRC/scrub-repair/latency-inflation) | risk band + median remaining hours |
| Workload | EWMA moments (size, read-frac, sync-frac, sequentiality) → 6-way cascade (Db/Log/Stream/Meta/Vm/Vhost) | policy retunes |

Design notes that matter:

- **Ransomware vs. compression** is the discriminating case: both are
  high-entropy. The watcher's rewrite-fraction and lure-extension
  signals carry that weight (entropy alone caps at 5000 bps);
  the 8000 freeze line is unreachable by a compression workload and
  reached in ~6 windows by a full-volume encrypt-in-place.
- **Escalation is never suppressed**: the advisory rate-limiter keys
  on (kind, band, device) — a worse band is a different key, so
  flapping detectors can't flood the bus *and* escalations always
  through.
- **Actions are reversible policy operations**: FreezeSnapshots
  (hold rotation so pre-encrypt recovery points survive),
  EscalateScrub, PlanMigration (rebalance, §12.2), RetunePolicies.
  Every advisory is logged with evidence; `drain()` is the bus.

The entropy math is integer fixed-point throughout (16-step quantized
log2, ~0.09 bits/byte worst error) — the simulator and the daemon
agree bit-for-bit, which is what makes the "small AI" auditable.

## 8. Observability (`src/telemetry/prometheus.rs`)

A dependency-free metrics registry that renders the Prometheus text
exposition format (0.0.4):

- `Histogram`: 49 log-linear buckets (1 µs → 36 min) + Inf,
  cumulative, with `_bucket{le=...}`/`_sum`/`_count` series;
  `quantile()` interpolates for humans.
- `Counter`/`Gauge` with saturation arithmetic.
- `Registry`: families with HELP/TYPE, label escaping (`\`, `"`,
  newline), deterministic order (family name, then labels) — a scrape
  must be diffable.
- Handles are `Rc` cells; `observe()` is one RefCell borrow on the
  completion path. The scraper pulls over the daemon's health socket.

Per-file IO latency (`lfs_io_latency_us{op,tier}`) is the flagship
series; quota denials, GC efficiency, Guardian advisories, and
rebalance progress all export through the same registry.

## 9. Migration (`src/migrate/`)

Strategy table (the whole decision):

| Source | Strategy | Why |
|--------|----------|-----|
| ext4/XFS/Btrfs/F2FS/ZFS, mounted | TarStream | tar carries semantics |
| NTFS | PerFile | alternate data streams |
| HFS+ | PerFile | resource forks |
| APFS | PerFile | named forks, clonefile |
| anything unmountable | RawBlock | carve, **operator sign-off required** |

Detection is a 10-rule magic table at documented offsets (ext4's
0xEF53@1080, XFS "XFSB"@0, Btrfs "_BHRfS_M"@0xFF00, ZFS label magic,
F2FS 0x0FF10FF0@1024, NTFS "NTFS    "@3, FAT32@82, exFAT@3, HFS+
"H+"/"HX"@1024, APFS "NXSB"@32).

The **manifest protocol** is the real feature: every imported file
gets a (path, size, SHA-256) ledger row; the import is not "done"
until every row re-verifies on the destination. `lfs_migrate` runs
detection, dry-run planning, and the demo matrix as its CI artifact.

## 10. Container & VM awareness (`src/container/`)

- **Layer CAS** (`layers.rs`): container image layers register by
  content digest; re-pulls are refcount bumps (`saved_bytes`
  accounting); new layers pin their chunks in the hot dedup index so
  sharing actually hits. Sharing ratio (logical/materialized, bps)
  exports to Prometheus; unreferenced layers sweep after the GC
  reclaims their extents.
- **Virtiofs passthrough** (`virtiofs.rs`): host-path → tag exports
  with cache-model (`none/auto/always`), DAX, and identity-squash
  policy. Tag collisions across paths are refused (the guest could
  not tell them apart). One page cache on the host; LionFS
  checksum/scrub still covers every byte the guest touches.

## 11. Key management (`src/security/kdf.rs`)

The hierarchy the 2.0 module called out as missing:

```text
passphrase ──PBKDF2-HMAC-SHA256 (600k iter, 128-bit salt)──► KEK
volume master (random 32 B) ──ChaCha20-Poly1305 wrap(KEK)──► on-disk blob
per-file key = HMAC-SHA256(master, "LFS3/file-key/v1" || file_id)
```

- PBKDF2 is hand-rolled over `sha2` (RFC 8018; known-answer tested)
  — the audit surface is 30 lines, and Windows keeps its zero-crate
  build.
- **Rewrap** rotates the passphrase without touching the master —
  re-key is metadata-only, because rotating the master re-derives the
  whole file-key tree without rewriting any data block.
- The live envelope zeroizes its master on drop via `write_volatile`
  (no `zeroize` crate). Secure-erase semantics: drop the envelope →
  the wrapped blob is noise. Physical block erasure is the GC's job
  and is *not* a microscope guarantee — the docs say so.
- Not implemented here (userspace tooling, §11.4): KMS clients, TPM
  sealing, recovery-key escrow.

## 12. Retention & pool evolution

### 12.1 Snapshot retention (`src/fs/retention.rs`)

GFS with tier budgets: 24 hourly / 14 daily / 8 weekly / 12 monthly /
3 yearly (defaults; operator-tunable). Selection is **additive** — a
snapshot serving as a day's representative is never also consumed by
the hourly budget. Calendar math is integer (Hinnant's
civil-from-days; the `y + (m <= 2)` adjustment is load-bearing) with
ISO-8601 week keys (Monday start, week 1 = first-Thursday week).
Output is a keep-set; everything else rides the ordinary snapshot
delete + GC reclamation path.

### 12.2 Online rebalance (`src/pool/rebalance.rs`)

Device add/remove without mkfs. Targets converge to the **capacity
proportional share** of pool usage (a 16 TiB device holds 4× a
4 TiB device's share), discounted by health: Watch −25%, Degraded
−50%, Failing −100% (drain) — rebalance doubles as the evacuation
path Guardian's advisories request. `leaving` devices drain to zero
and are never planned *into*. Moves are budget-sized per round
(default 1 GiB) and ride the CoW path in the Bulk class;
`is_balanced()` is the operator's "can I remove the device now"
check (±1% of capacity slack).

## 13. Testing strategy (what makes 3.0 trustworthy)

- **638 unit/property tests** (2.0: 462): every new module carries
  its table-driven tests, including the adversarial ones (torn tails,
  wrong passphrases, CRC corruption, flapping detectors, rate-limit
  keys, off-by-one calendar cases, WFQ fairness ratios).
- **Determinism rule**: no wall clock inside policy objects. QoS
  buckets, GC, Guardian, retention, rebalance all take
  caller-supplied time — the simulator (planned Phase 8) and
  production share bit-for-bit behavior.
- **Known-answer vectors**: PBKDF2 (RFC 8018 corpus), HMAC-SHA256
  (FIPS 198-1), entropy (exact powers-of-two cases), ISO week edges
  (2021-W53), superblock magics.
- Still ahead (ROADMAP Phase 8): the fault-injection simulator over
  the full 3.0 policy stack, xfstests port, journal TLA+.

## 14. Trade-offs (the 3.0 addenda to RFC-002 §10)

| Decision | Cost | Payoff | Verdict |
|----------|------|--------|---------|
| 256-bit optional plane | +4% insert / +2% lookup on Wide volumes | headroom for fabric pools, opt-in only | Compact default; Wide for pools |
| QoS in the submission path | ~30 ns/admission (two integer compares + token math) | 256:1 unfairness fixed; maintenance work yields | worth it at any tenant count > 1 |
| Record journal | one more write-before-tree discipline + checkpoint IO | small writes: 3 scattered ops → 1 sequential | net win below 4 KiB, exactly where it applies |
| Copy-GC | background copy IO (Bulk class) + census memory (24 B/segment) | CoW capacity leak bounded; wear leveled | the alternative is "leak forever" |
| Guardian userspace-only | can't act in-path (by design) | crash-testability preserved; still catches ransomware/drives/workloads | the honest split |
| WFQ over strict priority | latency class isolation is softer than RT scheduling | no starvation, tunable weights, 20 lines | correct for a filesystem, not an RTOS |
| PBKDF2 vs. Argon2 | GPU-attack cost per guess is lower | zero new deps, 30 auditable lines, Windows stays std-only | 600k iterations + AEAD tags; Argon2 is a drop-in later |
| Digest dedup for layers | digest computation on pull | 5-10× sharing on container hosts | composes with existing chunk dedup |

## 15. Phasing

- **Phase 7 (this release):** all policy layers + structures
  implemented and tested beside the live 2.0 paths, runnable through
  the four tools (`lfs_guardian/migrate/gc/retention`), metrics
  registry exportable.
- **Phase 8 (wiring):** switch the write path onto the record
  journal; GC-planner → scrubber → allocator loop; QoS admission into
  the shard dispatcher and group-commit pick; retention into the
  snapshot daemon; rebalance into the pool manager; Guardian onto the
  live telemetry socket; Prometheus onto the health socket; migration
  tooling onto the real tar stream. Each switch under the interleaved
  A/B measurement protocol (RFC-002 §2.4).
- **Phase 9 (scaling):** SPDK bypass plane (RFC-002 P6), CXL journal
  on real hardware, WinFsp bridge, the deterministic full-stack
  simulator.

---

*RFC-004 §15 closes with the same discipline it opened with: the 3.0
subsystems are consultative layers over an unchanged, crash-tested
substrate. Unlimited capacity is an option, not an obligation; the AI
is in the observatory, not the kernel; and every new promise in this
file is attached to a test that keeps it honest.*
