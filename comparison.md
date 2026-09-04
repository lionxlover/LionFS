# LionFS vs. Other Filesystems (At a Glance)

This card is the one-page summary; the canonical document with the
full caveats and the methodology discussion is
[`docs/comparison.md`](docs/comparison.md).

An earlier revision of this file presented a feature table backed by
fabricated benchmark numbers. Those numbers are gone. What remains is
qualitative: claims about what the LionFS code implements, verifiable
by reading it -- not by trusting a benchmark table.

## Feature parity (qualitative)

| Feature | LionFS | ext4 | XFS | Btrfs | ZFS |
|---------|--------|------|-----|-------|-----|
| End-to-end per-block checksums | yes (checksum tree) | no | no | yes | yes |
| Snapshots | yes (snapshot tree) | no | no | yes | yes |
| Built-in RAID profiles | 0/1/5/6/10 | no (md) | no (md) | yes | yes |
| Transparent compression | zstd clusters | no | no | yes | yes |
| Deduplication | tree exists; not wired | no | no | external tooling | yes |
| Encryption | per-inode AEAD | fscrypt | fscrypt | no | native |

Caveats that matter, stated once here and in full in the canonical
doc: the Markov read-ahead is wired but measured negative and ships
disabled by default; data-path copy-on-write infrastructure exists but
the write path does not use it; the dedup tree is not consulted by
writes. Each is listed as it is, not as a checkbox.

## Where LionFS sits in the design space

```mermaid
flowchart TB
    ROOT["filesystem design space"]
    ROOT --> INPLACE["in-place extents - ext4 XFS"]
    ROOT --> COW["CoW with checksums - Btrfs ZFS"]
    INPLACE --> LION["LionFS - journal RoW metadata plus in-place data"]
    COW --> LION
    LION --> F1["checksum tree"]
    LION --> F2["snapshot tree"]
    LION --> F3["RAID 0 1 5 6 10 plus RS erasure"]
    LION --> F4["zstd clusters and per-inode AEAD"]
    LION --> F5["stated gaps - dedup not wired and data CoW unused"]
```

## The one countable metric -- and its limits

Because the table is verifiable by code reading, it can be summarized
without lying. Verified-feature coverage of filesystem $A$ against
reference $B$:

$$C(A, B) = \frac{|F_A^{\mathrm{wired}} \cap F_B|}{|F_B|}$$

where $F^{\mathrm{wired}}$ counts only features the write path actually
uses -- "tree exists; not wired" scores zero. Counting the six rows:

$$C(\mathrm{LionFS}, \mathrm{ZFS}) = \frac{5}{6} \approx 0.83, \qquad
C(\mathrm{LionFS}, \mathrm{Btrfs}) = \frac{4}{4}$$

The metric counts rows in one table. It says nothing about maturity,
tooling, or performance, and must not be quoted as if it did.

## What a real comparison would require

```mermaid
flowchart LR
    A["mount LionFS via FUSE"] --> B["fio standard profiles"]
    B --> C["identical NVMe kernel and fio versions"]
    C --> D["ext4 XFS Btrfs ZFS matched mount options"]
    D --> E["report throughput and P99 P999 latency"]
    E --> F["pin exact versions in the writeup"]
```

Until that run exists, this document makes no performance comparison
at all -- the honest answer is "none measured, none claimed."
