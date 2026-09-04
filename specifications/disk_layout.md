# Disk_layout Specification

*This specification is planned for a future Phase of LionFS.*

## Planned layout (grounded in [`superblock.md`](superblock.md))

Region order follows the superblock pointers — `bitmap_start` and
`inode_table_start` locate the two metadata regions; data fills the
rest:

```mermaid
flowchart LR
    SB["block 0: superblock (4096 B)<br/>magic, version, geometry,<br/>region pointers"] --> BM["free-space bitmap region<br/>(bitmap_start):<br/>1 bit per 4 KiB block"]
    BM --> IT["inode table region<br/>(inode_table_start):<br/>256 B per inode, 16 per block"]
    IT --> DR["data region:<br/>directory blocks, extent targets,<br/>tail-packed blocks"]
```

## Metadata overhead

Per volume of $V$ bytes with $n_{\text{ino}}$ inodes:

$$B_{\text{meta}} = 4096 + \frac{V}{2^{15}} + 256\, n_{\text{ino}}$$

The bitmap term dominates the fixed costs — 1 bit per 4096-byte
block:

$$\frac{2^{40}\ \mathrm{B}}{2^{15}} = 2^{25}\ \mathrm{bits} = 32\ \mathrm{MiB}\ \text{per TiB} \approx 0.003\%\ \text{of the volume}$$

The inode table packs 16 records per block:

$$\left\lfloor \frac{4096}{256} \right\rfloor = 16 \quad\Rightarrow\quad B_{\text{inodes}} = \frac{256\, n_{\text{ino}}}{4096}\ \text{blocks}$$