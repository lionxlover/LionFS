# LionFS Performance Notes

An earlier version of this document claimed "extreme throughput and
sub-millisecond latencies" from a laundry list of micro-optimizations
(lock-free transaction generations, RCU-style tree operations,
per-CPU allocator caches, cache-line-aligned nodes, SIMD/AVX-512
checksums, "less than 1% CPU overhead" integrity). **None of those
claims were measured**, several described code that does not do what
the prose said (there is no per-CPU allocator cache in the write path;
checksums are not SIMD-dispatched), and the document has been
rewritten. Measured numbers live in `docs/benchmarks.md`; this page
describes what the code actually does on the hot paths after the
Phase 0-5 performance work, with each claim traceable to a commit.

## What actually changed on the hot paths (measured)

**Buffer handling** (P1.1): the read/write paths no longer materialize
a heap `Vec` copy of every block. Reads copy from the stack buffer
straight into the caller's slice; holes need no copy at all (output is
pre-zeroed). Writes hand the stack buffer to the transaction layer
directly; the cipher path transfers ownership via
`TxContext::write_block_owned` instead of copying. Measured: ~3% on
sequential read/write.

**Allocation** (P1.2/P1.3): the bitmap scan starts at a persistent
frontier cursor (`TxContext::alloc_cursor`, porting
`allocator::allocator::BlockAllocator`'s `last_allocated` tracking into
the live path) instead of rescanning all used bits. Appends allocate
one speculative run per write call (`allocator::extents::size_for_request`
semantics: blocks-needed + 25%), marking only the blocks actually
written -- the unmarked tail is a best-effort reservation that the
next append picks up, so extents merge across calls. Metadata
(checksum-tree nodes, spill-tree nodes) allocates from the END of the
block group (`allocate_extents_meta`), growing downward, so metadata
allocations do not puncture sequential data runs. Measured: a fresh
32 MiB sequential file went from 8192 extent fragments to 8; reads
+18-21%.

**RAID parity** (P3): RAID5/6 writes update parity incrementally
(read old data + old parity, XOR the delta into P; for Q, scale the
delta by the column's GF(256) coefficient) instead of re-reading the
whole stripe row. The GF hot path uses a precomputed 256-byte
multiplication table per coefficient. Journal replay keeps the full
recompute (idempotency requirement). Measured: random-write commit
+71% (RAID5-6dev) / +62% (RAID6-6dev); see benchmarks.md.

**Compression** (P4): zstd at the 128 KiB cluster granularity with
variable-length physical extents -- compression actually saves space
(ratio 2.90x on a mixed corpus) instead of padding each compressed
block back to 4 KiB. Level is a mount option.

## What is deliberately NOT claimed

- Readahead: the Markov predictor is wired but measured negative
  (-48%..-51% on reads in the in-process harness) and ships disabled
  by default. It is not listed as a working optimization.
- "Lock-free", "RCU", "SIMD/AVX-512", "per-CPU caches": none of these
  describe the current code. The transaction layer uses atomics for
  IDs; tree operations take no locks because the FUSE path is
  single-threaded per mount today, not because of lock-free design.
  Checksums (CRC32C, XxHash64, BLAKE3) use their crates' scalar paths.
- Any latency (P99/P999) numbers: no latency measurement exists in
  this repository.

## Where the remaining costs are

Measured, not guessed (see `docs/benchmarks.md` for methodology):
the checksum-tree insert is ~45% of write cost (per a
`--no-checksums` A/B); RAID6's Q-syndrome math is scalar GF(256) and
dominates RAID6 commit cost; the journal writes every dirty block
twice (journal + final location) by design for crash safety. These
are the honest starting points for future work.

## Hot-path flow (diagram)

The measured components in one graph -- every box is a claim from the
sections above, traceable to a commit:

```mermaid
flowchart TB
    W["pwrite call"] --> ZC["stack buffer - no heap Vec copy"]
    ZC --> SPEC["speculative run allocation - blocks plus 25 percent"]
    SPEC --> META["metadata zoning - checksum nodes at group end"]
    SPEC --> J["journal write - every dirty block by design"]
    J --> CS["checksum tree insert - XxHash64 per block"]
    CS --> PAR["RAID parity - incremental RMW delta"]
    PAR --> GCB["group commit batch - shared device flush"]
    GCB --> CK["checkpoint - root swap and superblock"]
```

## The cost model behind the numbers

Per-block write cost decomposes as

$$C_{\mathrm{write}} = C_{\mathrm{base}} + C_{\mathrm{csum}} + C_{\mathrm{parity}} + C_{\mathrm{journal}}$$

with the measured checksum share $C_{\mathrm{csum}} \approx 0.45\,C_{\mathrm{write}}$
(the `--no-checksums` A/B). The journal's cost is device bytes, not
CPU: every dirty block is written twice by design, a write
amplification of

$$A = \frac{W_{\mathrm{device}}}{W_{\mathrm{logical}}} = 2$$

before parity (RAID5 adds one parity block per $k$ data blocks) and
before compression (which divides physical bytes by the measured
ratio). The checksum share also bounds the payoff of any checksum
optimization: removing it entirely buys at most
$1/(1-0.45) \approx 1.8\times$, which is why the insert -- not the
copy paths P1.1 already fixed -- tops the remaining-cost list.

Queueing identities for the mounted system, stated as constraints
because they are not measured here: throughput is bounded by per-op
CPU cost $s$ and per-batch overhead $p$,

$$X \le \frac{1}{s + p}, \qquad X_N \le \frac{N}{Ns + p} = \frac{1}{s + p/N}$$

with $N$ the batch size, and Little's law ties queue depth to latency:

$$N_{\mathrm{inflight}} = X \cdot R$$

The amortization shape is visible in the one measured engine-level
figure: `lfs_engine` reports 707 MiB/s on 4 KiB writes with io_uring
(batched `io_uring_enter`, registered buffers) against a 115 MiB/s
threaded floor on the same host -- README and
`specifications/io_engine.md`; the shared 2-vCPU container bounds the
absolute values, the $\approx 6.1\times$ ratio is the $p/N$
signature.
