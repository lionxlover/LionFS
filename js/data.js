/* ==========================================================================
   LionFS — js/data.js
   Single source of truth for ALL site content. Edit this file to update
   the site — no HTML changes required.
   ========================================================================== */

/* ---- Site configuration -------------------------------------------------
   repoUrl: points every GitHub link (nav, docs, footer) at your repository.
   Replace "your-username" after forking, and GitHub Pages links will work.
   ------------------------------------------------------------------------ */
export const CONFIG = {
  repoUrl: "https://github.com/your-username/lionfs",
  version: "3.8.0",
  versionShort: "3.8",
  releaseName: "The Throughput Release",
  phase: "Phase 14",
  license: "MIT OR Apache-2.0",
  rustVersion: "1.75",
  tests: 840,
  tools: 57,
  updated: "2026",
};

export const gh = (path = "") => `${CONFIG.repoUrl}/${path.replace(/^\/+/, "")}`;

/* ---- Navigation ---------------------------------------------------------- */
export const NAV = [
  { id: "overview",    label: "Overview" },
  { id: "architecture", label: "Architecture" },
  { id: "performance",  label: "Performance" },
  { id: "compare",      label: "Compare" },
  { id: "releases",     label: "Releases" },
  { id: "guardian",     label: "Guardian" },
  { id: "tools",        label: "Tools" },
  { id: "quickstart",   label: "Quick start" },
  { id: "docs",         label: "Docs" },
];

/* ---- Hero ---------------------------------------------------------------- */
export const HERO = {
  badge: `v${CONFIG.version} · ${CONFIG.releaseName}`,
  titleA: "The file system that",
  titleB: "heals itself.",
  lead:
    "LionFS is a from-scratch, high-performance, <strong>self-healing universal file system</strong> written in Rust — " +
    "line-rate io_uring throughput, O(1) snapshots, end-to-end checksums on every read, and QoS'd multi-tenancy, " +
    "from <strong>one code base</strong> on Linux, macOS and Windows.",
  ctas: [
    { label: "Get started", href: "#quickstart", primary: true },
    { label: "View benchmarks", href: "#performance", primary: false },
  ],
  stats: [
    { value: "840", unit: "tests", label: "Green suite (±io_uring)" },
    { value: "568", unit: "MiB/s", label: "Seq write 4 KiB, +59%" },
    { value: "31", unit: "µs", label: "Snapshot create, O(1)" },
    { value: "256", unit: "-bit", label: "Opt-in capacity plane" },
  ],
  platforms: ["Linux", "macOS", "Windows"],
  terminal: {
    title: "lion@storage — lionfs " + CONFIG.versionShort,
    lines: [
      { type: "cmd",  text: "cargo build --release --features io_uring" },
      { type: "out",  text: "   Compiling lionfs v3.8.0" },
      { type: "cmd",  text: "sudo target/release/mkfs_lfs disk.img 1024" },
      { type: "out",  text: "   formatted 1024 MB · journal v2 · checksums: blake3" },
      { type: "cmd",  text: "sudo target/release/mount_lfs disk.img /mnt/lion" },
      { type: "out",  text: "   mounted at /mnt/lion · io_uring: on" },
      { type: "cmd",  text: "lfs_snapshot create /mnt/lion pre-upgrade" },
      { type: "out",  text: "   snapshot #1 created · O(1) · 31 µs", hl: true },
      { type: "cmd",  text: "lfs_simulate sweep" },
      { type: "out",  text: "   60 crash points · all invariants held", hl: true },
    ],
  },
};

/* ---- Ticker (keywords strip under hero) ---------------------------------- */
export const TICKER = [
  "io_uring", "WAL v2", "B-ε tree", "HAMT", "CoW", "birth generations",
  "ZNS", "SMR", "CXL-PMEM", "BLAKE3", "zstd", "FastCDC", "dedup",
  "O(1) snapshots", "RS(n,k)", "WFQ 8:4:1", "Guardian", "128-bit addressing",
  "FUSE", "WinFsp", "checksummed reads", "group commit", "reflink",
];

/* ---- Honesty banner ------------------------------------------------------- */
export const HONESTY = {
  title: "Honest by design",
  text:
    "LionFS is <strong>pre-alpha and unverified on real hardware</strong> — and every performance number on this site " +
    "comes from a command <strong>you can re-run yourself</strong> (<code>lfs_versus</code>, medians of 3). " +
    "No cross-filesystem number is quoted unless it was measured on the same host, in the same run.",
  chips: [
    { label: "840 tests · all green", cls: "good" },
    { label: "pre-alpha 3.8", cls: "warn" },
    { label: "numbers = re-runnable", cls: "acc" },
  ],
};

/* ---- Five pillars (the 2.0 architecture, RFC-002) ------------------------- */
export const PILLARS = [
  {
    numeral: "I",
    icon: "bolt",
    title: "I/O engine",
    tagline: "Line-rate submission from user space, on every platform.",
    points: [
      "io_uring backend: registered files, batched enter, kernel-side waits",
      "Portable threaded floor — same semantics everywhere",
      "Per-core shards, Vyukov MPMC queues, group commit (5 ms / 1 MiB windows)",
      "Zero-copy, lease-exclusive buffer arena",
    ],
    where: "src/<b>io_engine/</b>",
  },
  {
    numeral: "II",
    icon: "scale",
    title: "Scalability",
    tagline: "Addressing that outlives the hardware by 10¹⁰ ages of the universe.",
    points: [
      "128-bit volume addressing, packed 16-byte extents",
      "B-ε extent index with buffered leaves; persistent HAMT namespace",
      "v3 inode: inline small files ≤ 4032 B — one read, zero data blocks",
      "Tail packing and 4K/16K/64K universal alignment",
    ],
    where: "src/<b>addressing/</b> · src/<b>beepsilon/</b> · src/<b>hamt/</b>",
  },
  {
    numeral: "III",
    icon: "shield",
    title: "Reliability",
    tagline: "Checksum everything, trust nothing, repair autonomously.",
    points: [
      "Dual-speed checksums: xxHash64 hot, BLAKE3-128 cold, CRC32C structural",
      "Five-state mount recovery machine, fault-injection tested",
      "Autonomous repair planner + wired self-heal scrub",
      "Generalized RS(n,k) erasure coding, 200-round property-tested",
    ],
    where: "src/<b>recovery/</b> · src/<b>integrity/</b> · src/<b>pool/erasure.rs</b>",
  },
  {
    numeral: "IV",
    icon: "layers",
    title: "Media tiering",
    tagline: "Zoned, shingled, persistent — media-aware placement by policy.",
    points: [
      "ZNS zone-append policy: 85% switch point, WAF ≈ 1.0 (simulated)",
      "SMR band confinement + elevator sweeps; honest random-write rejection",
      "CXL-PMEM tier with CLWB",
      "Counted, never silent, alignment violations",
    ],
    where: "src/<b>media/</b>",
  },
  {
    numeral: "V",
    icon: "pipeline",
    title: "Data pipeline",
    tagline: "Compress, chunk, dedup — probe first, then pin.",
    points: [
      "Tiered compression: LZ4 / zstd-3 / zstd-12 / raw, probe-then-pin",
      "FastCDC content-defined chunking (2K / 8K / 32K)",
      "Three-level dedup index: bloom / hot-LRU / hash-tree at 0.1% RAM budget",
      "Punch-through escape on the 3rd RMW",
    ],
    where: "src/<b>pipeline/</b>",
  },
];

/* ---- Capability bento (3.0 additions + 3.8 throughput wins) --------------- */
export const CAPABILITIES = [
  {
    span: 6, feature: true, icon: "journal", ver: "3.8",
    title: "Journal v2 — the WAL rewrite",
    desc: "Segmented, byte-packed records (24 B header + block, contiguous) with per-segment footers. Journal traffic per 4 KiB block halves; the ring is written as a few coalesced pwrites instead of one syscall per block. v1 images upgrade in place — the FS_FEATURE_JOURNAL_V2 bit is stamped before the first v2 byte.",
    statValue: "8192 → 4120 B", statLabel: "journal traffic per 4 KiB block",
  },
  {
    span: 3, icon: "elide", ver: "3.8",
    title: "ZFS-mode data elision",
    desc: "Blocks a transaction allocated go straight to their final locations and sync in the same fsync as the journal leg — atomicity kept, journal spared.",
    statValue: "≈1.05×", statLabel: "write amplification (fresh-heavy)",
  },
  {
    span: 3, icon: "cache", ver: "3.8",
    title: "Shared node cache",
    desc: "One mount-wide 16k-node B-tree cache serves every tree descent — reads and writes. Before 3.8, each descent re-read its nodes from the device.",
    statValue: "+38%", statLabel: "cold sequential reads vs 3.7",
  },
  {
    span: 4, icon: "qos", ver: "3.0",
    title: "QoS & multi-tenancy",
    desc: "24 IO priority slots (Realtime / BestEffort / Bulk × 8), dual token buckets (bytes/s + ops/s), per-namespace quotas with grace windows, WFQ in virtual time — anti-laundering included.",
    statValue: "8 : 4 : 1", statLabel: "WFQ weights → 61.5% / 30.8% / 7.7%",
  },
  {
    span: 4, icon: "globe", ver: "3.0",
    title: "256-bit capacity plane",
    desc: "Opt-in WideAddr (mkfs-time): domain / namespace / volume / region / device / LBA + in-address byte offset for PMEM tiers. Lossless 128↔256 embedding; superblock plane gate.",
    statValue: "2²⁵⁶", statLabel: "≈ 1.2 × 10⁷⁷ addresses",
  },
  {
    span: 4, icon: "journal-sm", ver: "3.0",
    title: "Small-file record journal",
    desc: "Writes ≤ 4032 B become one sequential log write (40 B header + payload + CRC32) instead of three scattered device ops. Torn-tail replay, Commit/Checkpoint watermarks.",
    statValue: "3 → 1", statLabel: "device ops per small write",
  },
  {
    span: 3, icon: "gc", ver: "3.0",
    title: "Copy-GC",
    desc: "Rosenblum-Ousterhout cost/benefit, wear leveling, panic-mode watermarks, bounded plans, and honest all-live refusal.",
    statValue: "25 / 10%", statLabel: "kick / aggressive watermarks",
  },
  {
    span: 3, icon: "radar", ver: "3.0",
    title: "Guardian",
    desc: "Ransomware entropy watch, Weibull drive-failure prediction, 6-class workload classifier — all advisory, strictly out-of-band.",
    statValue: "0", statLabel: "actions on the data path",
  },
  {
    span: 3, icon: "chart", ver: "3.0",
    title: "Observability",
    desc: "Dependency-free Prometheus text exposition: 49-bucket log-linear latency histograms, counters, gauges — deterministic scrapes.",
    statValue: "19", statLabel: "bounded series on one object",
  },
  {
    span: 3, icon: "migrate", ver: "3.0",
    title: "Migration on-ramp",
    desc: "10-rule magic-byte detection (ext4 / XFS / Btrfs / ZFS / F2FS / NTFS / FAT32 / exFAT / HFS+ / APFS), SHA-256 manifest verification, sign-off gates.",
    statValue: "11/11", statLabel: "detection checks pass",
  },
  {
    span: 3, icon: "container", ver: "3.0",
    title: "Container & VM aware",
    desc: "Image-layer content-addressable store with refcounted sharing and hot-index pinning; virtiofs passthrough policy table (cache model / DAX / squash).",
    statValue: "CAS", statLabel: "image layers shared by hash",
  },
  {
    span: 3, icon: "key", ver: "3.0",
    title: "Key management",
    desc: "PBKDF2-HMAC-SHA256 (600k iterations) derives a KEK that wraps the volume master (ChaCha20-Poly1305). Per-file keys via HMAC-PRF — re-key is metadata-only. Volatile zeroizing envelope.",
    statValue: "600k", statLabel: "KDF iterations, mount lockout ×3",
  },
  {
    span: 3, icon: "clock", ver: "3.0",
    title: "Snapshot retention",
    desc: "GFS tier budgets with additive representative selection on a real civil/ISO-week calendar; interval-rate-limited passes; failed expirations retried.",
    statValue: "48h–7y", statLabel: "tuned GFS budgets (5 tiers)",
  },
  {
    span: 3, icon: "balance", ver: "3.0",
    title: "Pool evolution",
    desc: "Online rebalance with capacity-proportional targets, health-discounted evacuation (Guardian-integrated), drain-to-remove, budget-sized moves on the CoW path.",
    statValue: "online", statLabel: "no unmount, ever",
  },
];

/* ---- Architecture explorer ------------------------------------------------ */
export const ARCHITECTURE = {
  outOfBand: [
    { icon: "radar", label: "Guardian advisory bus", desc: "strictly out-of-band" },
    { icon: "zap",   label: "sim — deterministic crash simulator", desc: "power cuts at op indexes" },
  ],
  layers: [
    {
      name: "Applications",
      sub: "POSIX programs · containers · VMs",
      eyebrow: "Layer 01 · consumers",
      desc: "Ordinary software. Apps see a normal POSIX mount — create, read, write, lookup, readdir, rename, setattr, xattrs, fallocate, copy_file_range. Nothing about LionFS leaks through the interface except, occasionally, the speed.",
      chips: ["FUSE bridge (Linux / macFUSE)", "WinFsp bridge (Windows, RFC-003)"],
      where: "src/fuse/ · bridges hang off VfsOps",
    },
    {
      name: "VFS",
      sub: "the platform-neutral VfsOps surface",
      eyebrow: "Layer 02 · operations surface",
      desc: "One engine, one operations surface. The 3.4+ parallel path: &self ops, one mount, N threads — buffered writes land in a write-back intake page cache behind per-inode gates, metadata staging stays serialized, concurrent fsyncs coalesce into one journal run.",
      chips: ["&self operations", "write-back intake", "lock-free readers (seqlock)"],
      where: "src/vfs/",
    },
    {
      name: "Wiring",
      sub: "7 seams, each governing its own path",
      eyebrow: "Layer 03 · policy layers (3.1)",
      desc: "Every 3.0 policy sits exactly on the path it governs, behind a narrow seam whose contract is uniform: the engine owns the thread, the wiring owns the step, every decision is a pure function of caller-supplied time — and every switch carries A/B counters.",
      chips: ["qos_gate", "small_write", "gc_loop", "retention_daemon", "telemetry_bridge", "key_flow", "tar_stream"],
      where: "src/wiring/",
    },
    {
      name: "io_engine",
      sub: "per-core shards · MPMC queues · group commit",
      eyebrow: "Layer 04 · the engine",
      desc: "Vyukov MPMC queues feed per-core shards; group commit batches at 5 ms / 1 MiB windows and picks batches by WFQ virtual finish time. The lease-exclusive buffer arena hands out zero-copy views. This is where line-rate lives.",
      chips: ["io_uring", "threaded floor", "group commit 5ms/1MiB", "WFQ 8:4:1"],
      where: "src/io_engine/",
    },
    {
      name: "Index",
      sub: "B-ε extent index · HAMT namespace",
      eyebrow: "Layer 05 · metadata",
      desc: "The B-epsilon tree indexes extents with buffered leaves (25% padding); the persistent HAMT holds the namespace. Snapshot correctness is by construction: every B-tree node write carries a monotone stamp, and mutation paths path-copy frozen nodes before touching them. Birth generations in the checksum tree make snapshots O(1) total.",
      chips: ["B-ε tree", "HAMT", "v3 inode · inline ≤4032 B", "birth generations"],
      where: "src/beepsilon/ · src/hamt/ · src/ondisk/",
    },
    {
      name: "PAL",
      sub: "positioned I/O · sync flavors · geometry · CSPRNG",
      eyebrow: "Layer 06 · platform abstraction",
      desc: "The ONLY place Linux, macOS and Windows differ. fdatasync / F_FULLFSYNC / FlushFileBuffers, geometry probing, getrandom / getentropy / ProcessPrng, wake primitives. The Windows build pulls zero external crates.",
      chips: ["positioned I/O", "3 sync flavors", "CSPRNG via PAL", "wakers"],
      where: "src/pal/",
    },
    {
      name: "Backends",
      sub: "io_uring (Linux) · threaded (portable floor)",
      eyebrow: "Layer 07 · submission",
      desc: "The io_uring backend uses registered files, batched submissions and kernel-side waits. When the feature is off, the kernel refuses io_uring_setup, or the ring is unavailable — the engine degrades to the threaded backend with identical semantics.",
      chips: ["io_uring 0.7", "feature-gated", "graceful degradation"],
      where: "src/pal/io_uring.rs · src/pal/threaded.rs",
    },
    {
      name: "Media tiers",
      sub: "SSD · ZNS · SMR · CXL-PMEM",
      eyebrow: "Layer 08 · the metal",
      desc: "Zone-append policy for ZNS (85% switch, WAF ≈ 1.0), band confinement and elevator sweeps for SMR with honest random-write rejection, a CXL-PMEM tier with CLWB, and universal alignment with counted violations.",
      chips: ["SSD", "ZNS", "SMR", "CXL-PMEM + CLWB"],
      where: "src/media/",
    },
  ],
};

/* ---- Journey of a write --------------------------------------------------- */
export const WRITE_PATH = {
  steps: [
    {
      icon: "inbox", tag: "VFS write",
      title: "A write arrives",
      desc: "Through the real VfsOps surface — &self operations, one mount, N threads. The write lands in the write-back intake page cache behind per-inode gates; metadata staging stays serialized behind one staging lock.",
    },
    {
      icon: "gate", tag: "wiring::qos_gate",
      title: "Quota early-reject",
      desc: "Per-namespace quotas (with grace windows) get the first word — EDQUOT before any device I/O. Then dual token buckets meter bytes/s and ops/s with burst and lazy integer refill across 24 IO classes.",
    },
    {
      icon: "route", tag: "the switch",
      title: "Size router — 4032 bytes",
      desc: "The threshold is pure inode geometry: 4096 − 64 = 4032 bytes. Small writes take the record journal; large writes take the ordinary CoW path.",
      routes: [
        { title: "≤ 4032 B — journal", desc: "One sequential log write (40 B header + payload + CRC32) instead of three scattered device ops. Read-your-write overlay; checkpoint drain into the B-ε tree." },
        { title: "> 4032 B — CoW / elision", desc: "Redirect-on-write. In 3.8, blocks a transaction ALLOCATED are elided straight to their final locations and synced in the same fsync as the journal leg." },
      ],
    },
    {
      icon: "commit", tag: "group commit",
      title: "Group commit — WFQ picks the batch",
      desc: "Batches are picked by weighted fair queueing in virtual time (weights 8:4:1 → 61.5% / 30.8% / 7.7% service share). WAL v2 writes the ring as a few coalesced pwrites; the apply loop writes ascending contiguous runs.",
    },
    {
      icon: "shield", tag: "PAL barrier",
      title: "PAL durability barrier",
      desc: "fdatasync on Linux, F_FULLFSYNC on macOS, FlushFileBuffers on Windows — the PAL picks the strongest flavor each platform offers. Data AND metadata of the commit are atomic together.",
    },
    {
      icon: "zap", tag: "sim: crash cut", crash: true,
      title: "Power cut here?",
      desc: "The deterministic crash simulator cuts power at exact op indexes, seeded universes on a simulated clock. The 60-point sweep is an assertion suite: prefix property, overlay convergence — every invariant, every crash point.",
    },
    {
      icon: "replay", tag: "replay",
      title: "Replay on the next mount",
      desc: "Torn tails are detected and dropped; complete transactions replay in order. Blocks the cut transaction allocated are unreferenced orphans — the crash-safety argument is exact, not heuristic: a valid footer implies fresh data is durable; an absent footer means it never existed.",
    },
    {
      icon: "check", tag: "durable", finale: true,
      title: "Write durable — atomically",
      desc: "Every commit's data AND metadata landed together, checksum records batched (one node read + one node write per leaf), inode high-water marks persisted. This is the ZFS synchronous-write posture — kept, not traded, even at 568 MiB/s.",
    },
  ],
};

/* ---- Performance ----------------------------------------------------------- */
export const PERFORMANCE = {
  kpis: [
    { delta: "+59% vs 3.7", value: "568", unit: "MiB/s", label: "Seq write 4 KiB, buffered, fsync@end" },
    { delta: "21× vs ext4", value: "13", unit: "µs", label: "PUNCH_HOLE 4 KiB" },
    { delta: "O(1) total", value: "31", unit: "µs", label: "Snapshot create, live mount" },
    { delta: "all green", value: "840", unit: "tests", label: "With and without io_uring" },
  ],
  tabs: [
    {
      id: "improve",
      label: "3.7 → 3.8",
      note: "Same harness, same host, medians of 3. Every gain below is a fix you can find in the design record: specifications/phase14_wal_v2.md.",
      rows: [
        { wl: "Seq write 4 KiB (buffered, fsync@end)", unit: "MiB/s", v37: 357, v38: 568, note: "WAL v2 + data elision" },
        { wl: "Seq read 4 KiB, cold", unit: "MiB/s", v37: 257, v38: 355, note: "Shared node cache: every descent was hitting the device" },
        { wl: "Small files (create + write + fsync)", unit: "ops/s ×1000", v37: 12.7, v38: 15.1, note: "Batched checksum staging + WAL v2" },
        { wl: "Unlink (200 files)", unit: "ops/s ×1000", v37: 21.4, v38: 26.8, note: "Ascending-run apply loop" },
        { wl: "Journal traffic per 4 KiB block", unit: "bytes", v37: 8192, v38: 4120, note: "Segmented byte-packed records — lower is better", lowerBetter: true },
      ],
    },
    {
      id: "versus",
      label: "vs ext4 — same harness",
      note: "lfs_versus, 2-vCPU dev container, release build, medians of 3, per-run cache eviction on cold legs. Caveat printed with every run: LionFS is measured at ENGINE level (in-process VfsOps, no FUSE, no syscalls); ext4 through the kernel.",
      rows: [
        { wl: "Seq write 4 KiB (buffered, fsync@end)", unit: "MiB/s", lion: 568, ext4: 2404, note: "ext4 rides metadata-only journaling + async commit. LionFS keeps every commit's data AND metadata atomic — the ZFS-synchronous posture. 3.8's elision closes most of the gap anyway: +59%." },
        { wl: "Seq read 4 KiB — cold", unit: "MiB/s", lion: 355, ext4: 342, note: "3.8 node cache: AHEAD of ext4 in the same run, while verifying every block." },
        { wl: "Seq read 4 KiB — warm", unit: "MiB/s", lion: 2776, ext4: 7485, note: "ReadCache + 8-block prefetch (7.8× LionFS cold) vs the kernel's own page cache." },
        { wl: "Small files (create + write + fsync)", unit: "ops/s ×1000", lion: 15.1, ext4: 49.6, note: "Each LionFS file's commit is fully atomic with dual syncs." },
        { wl: "Random 4 KiB reads (2k)", unit: "MiB/s", lion: 12.1, ext4: 14.6, note: "Storage-bound parity — physics decides." },
        { wl: "PUNCH_HOLE 4 KiB", unit: "µs", lion: 13, ext4: 271, note: "21× faster. Punch is birth/pin-aware: a punch under a live snapshot retains the snapshot's bytes." },
      ],
      cantdo: [
        { value: "31 µs", label: "snapshot create — ext4 has no snapshots" },
        { value: "26 µs", label: "reflink clone, 0 bytes copied — ext4 has no reflinks" },
        { value: "live", label: "browse frozen snapshots on a live mount" },
      ],
    },
  ],
  engine: {
    title: "The engine under the mount",
    desc: "lfs_engine micro-benchmark, this host, re-runnable:",
    rows: [
      { label: "4 KiB writes · io_uring", value: "707 MiB/s" },
      { label: "4 KiB reads · io_uring", value: "1627 MiB/s" },
      { label: "64 KiB writes / reads · io_uring", value: "1268 / 3605 MiB/s" },
      { label: "4 KiB writes · threaded floor", value: "115 MiB/s" },
    ],
  },
  callout: "No performance claims beyond reproducible commands — the LFS-RFC-002 honesty rule, carried forward as a first-class constraint. Re-run everything yourself: <code>cargo build --release --bin lfs_versus --bin mkfs_lfs</code>, then <code>./target/release/lfs_versus --scratch-dir /tmp/vs --csv out.csv</code>.",
};

/* ---- Comparison matrix ------------------------------------------------------ */
export const COMPARISON = {
  systems: ["LionFS 3.8", "ext4", "XFS", "Btrfs", "ZFS", "NTFS", "ReFS", "APFS", "RedoxFS"],
  groups: [
    {
      group: "Integrity",
      features: [
        { f: "End-to-end per-block checksums", v: ["yes — checksum tree, every read verified", "no", "no", "yes", "yes", "no · integrity streams only", "yes · integrity streams", "no", "no"] },
        { f: "Crash consistency machinery", v: ["yes — journal + 5-state recovery + deterministic crash sim", "journal", "journal · metadata", "CoW + checksummed trees", "CoW + txg + ZIL", "journal", "CoW-ish · integrity streams", "CoW", "journal"] },
        { f: "Wired self-heal scrub", v: ["yes — reconstruct from parity, verify, rewrite", "no", "no", "scrub", "scrub", "no", "no", "no", "no"] },
      ],
    },
    {
      group: "Snapshots & replication",
      features: [
        { f: "Snapshots", v: ["yes — O(1)-total via birth generations", "no", "no", "yes", "yes", "VSS · out-of-band", "no · brackets only", "yes · APFS clones", "no"] },
        { f: "Snapshot browsing on live mount", v: ["yes — .lion/snapshots/<id>/, frozen reads", "no", "no", "no · snapdir off by default", "yes — .zfs/snapshot", "n/a", "n/a", "Time Machine · out-of-band", "no"] },
        { f: "Snapshot rollback", v: ["yes — NON-destructive, protective snapshot first", "n/a", "n/a", "yes · destructive", "yes · destructive", "n/a", "n/a", "n/a", "n/a"] },
        { f: "Incremental snapshot replication", v: ["yes — stream v2, digest-delta", "n/a", "n/a", "send/receive · whole-subvol", "yes — send -i", "robocopy / blocks", "no", "replicator", "n/a"] },
      ],
    },
    {
      group: "Data services",
      features: [
        { f: "Built-in RAID / erasure profiles", v: ["yes — 0/1/5/6/10 + RS(n,k)", "no · md", "no · md", "yes", "yes + RAIDZ", "no · dynamic disk", "no · storage spaces", "no · CoreStorage", "no"] },
        { f: "Transparent compression", v: ["yes — zstd / LZ4 clusters, probe-then-pin", "no", "no", "yes", "yes", "no", "no", "yes · transparent", "no"] },
        { f: "Deduplication", v: ["yes — wired, verify-on-share, opt-in", "no", "no", "no · out-of-band", "yes", "no", "yes · block clone", "yes · clone-based", "no"] },
        { f: "Encryption", v: ["yes — per-file AEAD, key tree, PBKDF2 envelope", "fscrypt · out-of-band", "fscrypt", "no", "native", "EFS · per-file", "BitLocker · volume", "native", "no"] },
        { f: "Reflink / clone files", v: ["yes — copy_file_range whole-file", "no", "reflink=1 · XFS v5", "yes", "no · different granularity", "no", "block clone · 2022+", "yes", "no"] },
      ],
    },
    {
      group: "POSIX & operations",
      features: [
        { f: "POSIX ACLs (1003.1e draft-17)", v: ["yes — ext4 wire format", "yes", "yes", "yes", "yes · NFSv4-style", "no · transformed", "no", "no · different model", "no"] },
        { f: "Sparse files + punch + SEEK_HOLE/DATA", v: ["yes — birth-aware punch", "yes", "yes", "yes", "yes · holes, no punch", "yes", "no", "yes", "no"] },
        { f: "fs-verity-style seals", v: ["yes — BLAKE3 root + enforced immutable", "yes", "no", "yes", "no", "no", "integrity streams · related", "no", "no"] },
        { f: "User quotas", v: ["yes — uid space + inode, EDQUOT", "yes", "yes", "yes · subvol/group", "yes · user/group/project", "yes", "yes", "yes · per-volume", "no"] },
      ],
    },
    {
      group: "Advanced",
      features: [
        { f: "Ransomware / drive-failure prediction", v: ["yes — Guardian, advisory, out-of-band", "no", "no", "no", "no", "no", "no", "no", "no"] },
        { f: "Multi-tenant QoS on the data path", v: ["yes — 24-slot classes, dual token buckets, WFQ", "no · ionice only", "no", "no", "no · io throttling out-of-band", "no", "no", "no", "no"] },
        { f: "Capacity addressing", v: ["128-bit standard, 256-bit opt-in", "64-bit", "64-bit", "64-bit", "64-bit+", "64-bit", "128-bit · 16EB claim", "64-bit", "64-bit"] },
        { f: "Platforms from one code base", v: ["Linux / macOS / Windows · PAL; Windows = zero external crates", "Linux", "Linux", "Linux", "many · ports", "Windows", "Windows", "Apple", "Redox"] },
        { f: "Field miles (the honest row)", v: ["pre-alpha, 840 tests, unverified on hardware", "millions of users", "millions of users", "millions of users", "millions of users", "millions of users", "enterprise", "billions of devices", "small"] },
      ],
    },
  ],
  quote: "Read that last row twice: it is the honest one. Every competitor has field miles LionFS does not have. The rows above say what the <b>code</b> implements (verifiable by reading it); the last row says what the <b>project</b> is — a pre-alpha engine with a large test suite, not a production filesystem.",
  caveats: "Caveats that matter, stated once: dedup is opt-in (ZFS's own posture); encrypted inodes are refused by send (rollback restores them); snapshot browsing is addressable up to 2²⁰ inodes per snapshot; the FUSE bridge is single-loop (fuser 0.12).",
};

/* ---- Releases timeline ------------------------------------------------------- */
export const RELEASES = [
  { ver: "0.1.0", name: "Initial prototype", tests: 0, minor: true, desc: "The first extent-based layout, block allocator and FUSE mount.", hi: ["extents", "FUSE", "mkfs_lfs"] },
  { ver: "1.x", name: "Line folded into 2.0", tests: 245, minor: true, desc: "Core POSIX surface, journaling, checksums, RAID 0/1/5/6/10 — 245 tests at the fold.", hi: ["245 tests"] },
  { ver: "2.0", name: "Cross-platform architecture", tests: 462, major: true, desc: "The PAL, the io_uring engine, 128-bit addressing, B-ε + HAMT, media tiering, the data pipeline.", hi: ["PAL", "io_uring", "B-ε", "HAMT", "ZNS/SMR", "FastCDC"] },
  { ver: "3.0", name: "Unlimited — RFC-004", tests: 638, major: true, desc: "Eleven consultative subsystems over the unchanged substrate: QoS, capacity plane, Guardian, migration, key management and more.", hi: ["11 subsystems", "256-bit plane", "Guardian"] },
  { ver: "3.1", name: "The wiring", tests: 713, major: true, desc: "Seven seams put every policy on the path it governs. The deterministic crash simulator arrives; tuned defaults land.", hi: ["7 seams", "crash sim", "WFQ 8:4:1"] },
  { ver: "3.2", name: "Data-path CoW", tests: 730, minor: true, desc: "Snapshots PIN data blocks; dedup wired (BLAKE3, verify-on-share); checksum fast-append cache; p50/p99/p999 reporting.", hi: ["data CoW", "dedup wired"] },
  { ver: "3.3", name: "Metadata path-copy CoW", tests: 736, minor: true, desc: "Snapshots freeze metadata by construction — O(1) in metadata. The mounted-fio framework ships.", hi: ["path-copy", "O(1) meta snapshots"] },
  { ver: "3.4", name: "Parallel write path", tests: 754, minor: true, desc: "One mount, N threads: write-back intake, per-inode gates, coalesced group commit. Intake 3036 → 4292 MiB/s at 2 jobs.", hi: ["&self ops", "group commit"] },
  { ver: "3.5", name: "Pipelined transaction groups", tests: 760, major: true, desc: "Quiesce in microseconds, commit without the staging lock, lock-free readers — and O(1)-TOTAL snapshots via birth generations. The flake hunt found five real bugs; all fixed with regression tests.", hi: ["txg pipeline", "O(1) snapshots", "5 bugs fixed"] },
  { ver: "3.6", name: "POSIX completeness I", tests: 790, major: true, desc: "Xattrs + ACLs, reflink clones, the WIRED self-heal scrub, crypto/format agility, the Format Vault, snapshot send/recv.", hi: ["ACLs", "reflink", "self-heal", "Format Vault"] },
  { ver: "3.7", name: "POSIX access & the cache plane", tests: 823, major: true, desc: "Symlinks, .lion/snapshots/ browsing, non-destructive rollback, incremental send, sparse files, dentry + read caches (10.3× warm), quotas, verity seals.", hi: ["snapshot access", "cache plane", "rollback"] },
  { ver: "3.8", name: "The throughput release", tests: 840, major: true, current: true, desc: "WAL v2, ZFS-mode data elision, the shared node cache, batched checksum staging, inode durability. +59% seq writes, +38% cold reads, one severe corruption window closed by construction.", hi: ["WAL v2", "elision", "node cache", "840 tests"] },
];

/* ---- Guardian ------------------------------------------------------------------ */
export const GUARDIAN = {
  lead: "Guardian is the autonomous-operations layer — watching entropy curves, drive health and workload shape, and advising. It never touches the data path: the advisory bus is escalation-safe, rate-limited, and strictly out-of-band.",
  cards: [
    {
      icon: "virus",
      title: "Ransomware entropy watch",
      desc: "Shannon entropy + rewrite rate + lure-file EWMAs. In the end-to-end sim: quiet workload → ransomware signature → <b>freeze advisory at window 14</b>.",
    },
    {
      icon: "pulse",
      title: "Weibull drive-failure prediction",
      desc: "Weibull hazard model with telemetry multipliers. The degrading-drive scenario emits a migration advisory with <b>~360 days of headroom</b> — evacuation before failure.",
    },
    {
      icon: "cpu",
      title: "6-class workload classifier",
      desc: "Classifies the live workload (Db, batch, log, quiet…) and emits a retune advisory when it shifts — QoS rates re-pinned without a remount.",
    },
  ],
  radarCaption: "lfs_guardian sim — quiet → ransomware → degrading drive → workload shift",
  foot: "Try it end-to-end: <code>lfs_guardian sim</code> runs the full pipeline — zero actions touch the data path.",
};

/* ---- Tools ---------------------------------------------------------------------- */
export const TOOLS = {
  categories: ["all", "benchmark", "format", "mount", "snapshots", "replication", "ops", "media", "integrity", "debug"],
  featured: [
    { cmd: "lfs_versus", cat: "benchmark", desc: "The same-harness, same-host, head-to-head vs ext4 (medians of 3, CSV). The engine-level half of the full comparison run." },
    { cmd: "lfs_simulate", cat: "integrity", desc: "The deterministic crash simulator's front door. Power cuts at exact op indexes; sweep asserts every invariant at every crash point." },
    { cmd: "lfs_engine", cat: "benchmark", desc: "I/O engine micro-benchmark and backend prober — io_uring vs threaded, 4 KiB to 1 MiB." },
    { cmd: "lfs_ioperf", cat: "benchmark", desc: "In-process I/O core benchmark with measured p50 / p99 / p999 per-call latencies." },
    { cmd: "lfs_smpbench", cat: "benchmark", desc: "SMP read scaling through the shared node cache — interleaved baseline/parallel in one process." },
    { cmd: "mkfs_lfs", cat: "format", desc: "Format images from 1024 MB up, single device or multi-device RAID (0/1/5/6/10, RS(n,k) pools)." },
    { cmd: "lfs_conformance", cat: "format", desc: "The Format Vault image checker: the 11-check conformance battery, offline. Caught two real bugs on day one." },
    { cmd: "lfs_upgrade", cat: "format", desc: "Offline, conformance-gated upgrade — stamps feature flags durably before new-format bytes land." },
    { cmd: "mount_lfs", cat: "mount", desc: "Mount images via FUSE (Linux/macFUSE) through the platform-neutral VfsOps surface." },
    { cmd: "lfs_palinfo", cat: "mount", desc: "Platform capability report + PAL self-test. Runs on all three OSes — the CI artifact that proves portability." },
    { cmd: "lfs_snapshot", cat: "snapshots", desc: "Real snapshot lifecycle: create / delete / list / verify through the actual SnapshotManager and journal. O(1) total." },
    { cmd: "lfs_clone", cat: "snapshots", desc: "Reflink clones — copy_file_range on a whole-file request shares physical blocks under refcount pinning." },
    { cmd: "lfs_fallocate", cat: "snapshots", desc: "fallocate (alloc / KEEP_SIZE / PUNCH_HOLE) and lseek(SEEK_DATA/SEEK_HOLE) on real images." },
    { cmd: "lfs_replicate", cat: "replication", desc: "Snapshot send|recv — full and incremental (stream v2, digest-delta, per-file SHA-256 verification)." },
    { cmd: "lfs_guardian", cat: "ops", desc: "The Guardian agent runner — the full advisory pipeline end-to-end in sim." },
    { cmd: "lfs_gc", cat: "ops", desc: "Copy-GC planner inspection across all three watermark bands, plus the wear-leveling demo." },
    { cmd: "lfs_retention", cat: "ops", desc: "GFS retention verdicts over synthetic history — 83 snapshots → 42 kept / 41 expired, tier by tier." },
    { cmd: "lfs_migrate", cat: "ops", desc: "Foreign-filesystem detection & import planning — 11/11 checks, dry-run plans with sign-off gates." },
    { cmd: "lfs_zns", cat: "media", desc: "ZNS/SMR zone & band policy inspector and simulator — WAF 1.000, 83% avg fill." },
    { cmd: "lfs_scrub", cat: "integrity", desc: "The wired self-heal scrub: verify every checksum record, reconstruct from parity, rewrite in place." },
    { cmd: "lfs_verify", cat: "integrity", desc: "fs-verity-style seals — BLAKE3 root sealed into a reserved xattr with the enforced immutable flag." },
    { cmd: "lfs_dump", cat: "debug", desc: "Superblock and inode inspection for images on the bench." },
  ],
  allBinaries: [
    "lfs_admin", "lfs_analyze", "lfs_balance", "lfs_bench", "lfs_benchmark", "lfs_clone", "lfs_compress",
    "lfs_conformance", "lfs_debug", "lfs_debug_btree", "lfs_dedupe", "lfs_device", "lfs_dump", "lfs_encrypt",
    "lfs_engine", "lfs_failover", "lfs_fallocate", "lfs_fsck", "lfs_gc", "lfs_guardian", "lfs_health",
    "lfs_inspect", "lfs_ioperf", "lfs_keys", "lfs_migrate", "mkfs_lfs", "lfs_monitor", "lfs_optimize",
    "lfs_palinfo", "lfs_policy", "lfs_pool", "lfs_predict", "lfs_profile", "lfs_raid", "lfs_rebuild",
    "lfs_recommend", "lfs_repair", "lfs_replicate", "lfs_report", "lfs_retention", "lfs_scheduler",
    "lfs_scrub", "lfs_security", "lfs_simulate", "lfs_smpbench", "lfs_snapshot", "lfs_stress",
    "lfs_telemetry", "lfs_upgrade", "lfs_verify", "lfs_versus", "lfs_volume", "lfs_zns", "mount_lfs",
  ],
};

/* ---- Quickstart ------------------------------------------------------------------- */
export const QUICKSTART = {
  steps: [
    {
      title: "Get the toolchain",
      desc: "Rust 1.75 or newer. That's the whole prerequisite list — the Windows build even pulls zero external crates.",
      lang: "shell",
      code: `# https://rustup.rs — Rust 1.75+
cargo --version
rustc --version`,
    },
    {
      title: "Build it",
      desc: "The portable build works everywhere; the io_uring fast path is a Linux feature flag. Same code, same semantics.",
      lang: "shell",
      code: `cargo build --release                      # portable everywhere
cargo build --release --features io_uring   # Linux fast path`,
    },
    {
      title: "Run the suite",
      desc: "840 library tests, green with and without io_uring — plus property tests and the crash simulator's determinism proof.",
      lang: "shell",
      code: `cargo test                 # 840 lib tests
cargo test --features io_uring   # same, through the ring
cargo bench               # criterion: beepsilon, fastcdc, btree, allocator, io`,
    },
    {
      title: "Format an image",
      desc: "A single-device image, or a multi-device RAID pool — same tool, same on-disk format family.",
      lang: "shell",
      code: `# single device (size in MB)
sudo target/release/mkfs_lfs disk.img 1024

# RAID5 across four devices
sudo target/release/mkfs_lfs dev0.img 1024 --raid raid5 dev1.img dev2.img dev3.img`,
    },
    {
      title: "Mount it",
      desc: "Through FUSE on Linux/macOS, via the platform-neutral VfsOps surface. WinFsp on Windows follows RFC-003.",
      lang: "shell",
      code: `sudo target/release/mount_lfs disk.img /mnt/lion

# RAID pool mount
sudo target/release/mount_lfs dev0.img /mnt/lion dev1.img dev2.img dev3.img`,
    },
    {
      title: "Prove crash-safety to yourself",
      desc: "The exit test: watch every crash point pass. Then exercise real workloads before trusting it with anything.",
      lang: "shell",
      code: `./target/release/lfs_simulate sweep

# head-to-head vs ext4 on YOUR hardware
cargo build --release --bin lfs_versus --bin mkfs_lfs
./target/release/lfs_versus --scratch-dir /tmp/vs --csv out.csv`,
    },
  ],
};

/* ---- Docs --------------------------------------------------------------------------- */
export const DOCS = [
  { icon: "book", title: "README.md", desc: "The honest overview — what the tree actually implements, deliberately not advertising what isn't there.", path: "README.md" },
  { icon: "layers", title: "docs/", desc: "Architecture deep-dives: platform support, io engine, addressing, media tiering, pipeline, reliability, RCU.", path: "docs/" },
  { icon: "scroll", title: "docs/rfc/", desc: "The normative RFCs: 002 architecture, 003 cross-platform, 004 unlimited, 005 format vault.", path: "docs/rfc/" },
  { icon: "grid", title: "specifications/", desc: "On-disk and subsystem specs — 40+ design records from addressing to wiring, incl. phases 9–14.", path: "specifications/" },
  { icon: "map", title: "ROADMAP.md", desc: "P1–P14 phases against the RFC program, with exit criteria and what's still open.", path: "ROADMAP.md" },
  { icon: "hammer", title: "BUILD.md", desc: "Per-platform build details for Linux, macOS and Windows.", path: "BUILD.md" },
  { icon: "clock", title: "CHANGELOG.md", desc: "Every release, every fix — Keep-a-Changelog format, all notable changes since the prototype.", path: "CHANGELOG.md" },
  { icon: "scale", title: "comparison.md", desc: "The full feature matrix and the one measured performance table, caveats included.", path: "comparison.md" },
];

/* ---- Final CTA & footer --------------------------------------------------------------- */
export const CTA = {
  title: "Build it. Run the 840. Then decide.",
  text: "LionFS earns trust the slow way: reproducible numbers, money tests that found real bugs, and a crash simulator that wins every argument with the hardware.",
  cmd: "cargo test",
  actions: [
    { label: "View on GitHub", href: "", primary: false, external: true },
    { label: "Get started", href: "#quickstart", primary: true },
  ],
};

export const FOOTER = {
  tagline: "A from-scratch, self-healing universal file system in Rust. Pre-alpha, honest numbers only.",
  columns: [
    {
      head: "Project",
      links: [
        { label: "Repository", path: "" },
        { label: "Issues", path: "issues" },
        { label: "Releases", path: "releases" },
        { label: "License — MIT OR Apache-2.0", path: "LICENSE" },
      ],
    },
    {
      head: "Docs",
      links: [
        { label: "README", path: "README.md" },
        { label: "RFCs (normative)", path: "docs/rfc/" },
        { label: "Specifications", path: "specifications/" },
        { label: "Roadmap", path: "ROADMAP.md" },
      ],
    },
    {
      head: "Community",
      links: [
        { label: "Contributing", path: "CONTRIBUTING.md" },
        { label: "Code of conduct", path: "CODE_OF_CONDUCT.md" },
        { label: "Security policy", path: "SECURITY.md" },
        { label: "Porting guide", path: "PORTING.md" },
      ],
    },
  ],
  status: `LionFS ${CONFIG.version} · ${CONFIG.license} · status: pre-alpha — unverified on real hardware.`,
  credit: `Static showcase · zero build step · ${CONFIG.updated}`,
};

