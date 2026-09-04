# Compression Specification

*This specification is planned for a future Phase of LionFS.*

## Codec tier policy (planned surface)

The implemented machinery is specified in
[`compression_pipeline.md`](compression_pipeline.md); this page will
codify the codec-level policy. The decision the engine makes per file:

```mermaid
flowchart TB
    P["first two clusters written<br/>probe: ClusterMeasurement<br/>(compressibility + encode latency)"] --> D{"decide"}
    D -->|"encode throughput below 250 MiB/s"| HOT["Hot: LZ4 block<br/>(latency-sensitive)"]
    D -->|"bulk default"| WARM["Warm: zstd-3<br/>(1.x measurement: 2.90x at 407 MiB/s)"]
    D -->|"ratio-first, idle window, pin_cold"| COLD["Cold: zstd-12"]
    D -->|"measured ratio below 1.2"| RAW["Raw: no codec, RAW flag<br/>(the honest fallback)"]
```

## Ratio math

With compression ratio $\rho = s_{\mathrm{orig}} / s_{\mathrm{comp}}$,
per-file savings are:

$$\Delta = \left(1 - \frac{1}{\rho}\right) s_{\mathrm{orig}}$$

Over a workload of $n$ written bytes, fraction $p$ compressible at
mean ratio $\bar\rho$, the expected bytes saved:

$$E = p \left(1 - \frac{1}{\bar\rho}\right) n$$

The raw fallback fires when $\rho < 1.2$ — forgone saving at most
$1 - 1/1.2 \approx 16.7\%$, below which codec CPU and RMW
amplification cost more than the space.