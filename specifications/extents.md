# Extent Specification

Extents map a contiguous range of logical file offsets to a contiguous range of physical blocks. 

## Structure (Little Endian)

| Offset (Hex) | Size | Name | Description |
|---|---|---|---|
| 0x00 | 8 bytes | `logical_start`| The starting block index within the file |
| 0x08 | 8 bytes | `physical_start`| The absolute starting block on disk |
| 0x10 | 8 bytes | `length` | The number of contiguous blocks in this extent |

Total Size: 24 bytes.

In Phase 1, up to 7 extents can be stored inline inside an Inode. In future phases, LionFS will support B-Tree extents.
