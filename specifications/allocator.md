# Allocator Specification

LionFS utilizes a **bitmap-based block allocator**.

## Design
- 1 bit represents 1 block (4096 bytes).
- `0` means free, `1` means allocated.
- A 4096-byte bitmap block can track 32,768 data blocks (134 MB).
- The allocator uses first-fit, contiguous scanning to fulfill extent requests, drastically reducing fragmentation compared to random allocation schemas.
