# Specification: Guardian — Autonomous Operations Agent (LionFS 3.0)

Status: implemented (`src/guardian/`) | RFC: LFS-RFC-004 §7

## The one rule

No model in the data path. The kernel side stays deterministic and
crash-testable; the agent observes telemetry and emits reversible
advisories. Every action is a policy operation (freeze snapshots,
escalate scrub, plan migration, retune policies) through ordinary
control-plane APIs, logged with evidence.

## Detectors

### 7.1 Entropy watch (ransomware)

Rolling evidence per watched tree, EWMA α=0.25, integer 32.32
fixed-point throughout:

| Signal | Weight | Evidence |
|--------|--------|----------|
| Shannon entropy (256-symbol histogram, 16-step quantized log2, ≤0.09 bits error) | 0.5 | saturates at 7.5 bits/byte |
| Rewrite fraction | 0.3 | rewrites / writes |
| Lure-extension fraction | 0.2 | `.doc/.docx/.xls/.xlsx/.ppt/.pptx/.pdf/.jpg/.jpeg/.png/.csv/.db` |

Combined score in bps; freeze at 8000. The discriminating case —
compressed output is high-entropy *like* ciphertext — is resolved by
the rewrite/lure signals: entropy alone caps the score at 5000, so a
compression workload (new files, no lures) never freezes, while a
full-volume encrypt-in-place reaches the line in ~6 windows.

The score is an EWMA over per-window evidence, integer 32.32
fixed-point throughout:

$$S_t = \alpha\, x_t + (1 - \alpha)\, S_{t-1}, \qquad \alpha = 0.25$$

Evidence half-life: $\ln 2 / \ln(4/3) \approx 2.4$ windows. The
per-window evidence is the weighted table, $x_t = 10^{4}\,(0.5\,e_H + 0.3\,e_R + 0.2\,e_L)$ bps with $e_H$ saturated at
$H = 7.5$ bits/byte — so entropy alone caps $x$ at 5000 bps, and
convergence from zero is geometric:

$$S_k = x\,(1 - 0.75^{k}) \;\Rightarrow\; k \approx 5.6\ \text{windows to reach 8000 from}\ x = 10^{4}$$

### 7.2 Drive-failure prediction

Two deliberately separate signals:

- **Telemetry multiplier** (100 = clean) drives the risk band:
  +40/event realloc, +80/pending sector, +10/CRC error, +60/scrub
  repair, +5 per latency-inflation point (p99/median above 2.0, one
  point per +0.2). Bands: Healthy <150, Watch <400, Degraded <1000,
  Failing ≥1000.
- **Weibull baseline** h(t) = k·(8760/η)·(t/η)^(k−1) with k=1.30,
  η=80,000 h only modulates the *remaining-life* estimate (median =
  ln 2 / effective annual hazard, saturated at 100 years). Age is a
  prior, not an alarm.

Output: band, multiplier, annualized effective hazard, estimated
median remaining hours — "migrate within days" for Failing, weeks
for Degraded.

The baseline in notation (per-hour hazard; the spec's form above is
the annualized $8760\,h(t)$):

$$h(t) = \frac{k}{\eta} \left(\frac{t}{\eta}\right)^{k-1}, \qquad k = 1.30, \quad \eta = 8 \times 10^{4}\ \mathrm{h}$$

feeding the saturated median remaining-life estimate:

$$t_{\text{med}} = \frac{\ln 2}{h_{\mathrm{yr,eff}}}, \qquad t_{\text{med}} \le 100\ \text{years}$$

### 7.3 Workload classifier

EWMA moments (mean IO size clamped at 1 MiB, read fraction, sync
fraction, sequentiality = max-run/bytes) over window aggregates; a
most-specific-first cascade:

1. size < 1 KiB ∧ seq < 20% → `Meta` (record-log path priority)
2. size ≥ 256 KiB ∧ read ≥ 80% ∧ seq ≥ 50% → `Stream` (readahead
   pinned, cold zstd-12)
3. size ≥ 256 KiB ∧ read < 20% ∧ seq ≥ 80% → `Log` (append grouping,
   LZ4, wide commit windows)
4. sync ≥ 5% ∧ 4..64 KiB → `Db` (punch-through armed, WAL on fast
   tier, no compression on WAL region)
5. 4..64 KiB ∧ seq < 20% → `Vm` (dedup on — page-sparse backing)
6. else → `Vhost` (defaults; honest fallback)

## The agent

`Agent::tick()` per window; `observe_suspicion / observe_drive /
observe_workload` feed evidence; `drain()` is the advisory bus
(bounded ring, default 256). Rate limiting keys on
(kind, band-or-class, device) — **escalation is never suppressed**:
a worse band is a different key. Half-suspicion (≥ freeze/2) emits
LogOnly so operators see the ramp.

Advisories: `RansomwareSuspicion / DriveRisk{band} /
WorkloadShift{class}` × `FreezeSnapshots / EscalateScrub /
PlanMigration / RetunePolicies / LogOnly`.

The advisory bus, end to end:

```mermaid
flowchart TB
    subgraph DET["detectors (pure functions of evidence)"]
        EN["7.1 entropy watch<br/>EWMA, 32.32 fixed-point"]
        DR["7.2 drive prediction<br/>telemetry multiplier + Weibull prior"]
        WC["7.3 workload classifier<br/>EWMA moments, first-match cascade"]
    end
    DET --> TK["Agent::tick per window<br/>(observe_suspicion / observe_drive / observe_workload)"]
    TK --> BUS["advisory bus: bounded ring (256),<br/>rate-limited per (kind, band-or-class, device),<br/>escalations never suppressed"]
    BUS --> ADV["advisories: RansomwareSuspicion,<br/>DriveRisk, WorkloadShift"]
    ADV --> ACT["policy actions, control plane only:<br/>FreezeSnapshots / EscalateScrub /<br/>PlanMigration / RetunePolicies / LogOnly"]
    BUS --> SOCK["telemetry socket: advisory stream<br/>+ evidence gauges"]
    ACT --> LOG["logged with evidence -<br/>no model in the data path"]
```

The socket side is the telemetry bridge in [`wiring.md`](wiring.md).

## Kept fixed

- The flapping-detector suppression bug: single-key rate limiting
  ate *escalations* (Healthy LogOnly one window suppressed the next
  window's Degraded advisory). Keys now carry the discriminating
  data.
- Integer fixed-point everywhere (the first draft's entropy table
  was numerically wrong; the rewrite uses exact powers-of-two tests
  and a 16-step log2 with stated error bounds).
- Wrong passphrase / corrupt envelope are audible failures (AEAD
  tags), and every guess costs the KDF 600k SHA-256s — see
  `key_management.md`.
