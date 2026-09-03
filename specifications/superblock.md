# Superblock Specification

The LionFS Superblock is located at physical block 0 (offset 0x00) and describes the global parameters of the filesystem.

## Structure (Little Endian)

| Offset (Hex) | Size | Name | Description |
|---|---|---|---|
| 0x00 | 4 bytes | `magic` | LionFS Magic Number (`0x4C494F4E` / "NOIL") |
| 0x04 | 4 bytes | `version` | Filesystem Version (currently 1) |
| 0x08 | 8 bytes | `total_blocks` | Total number of blocks in the volume |
| 0x10 | 8 bytes | `free_blocks` | Number of currently free blocks |
| 0x18 | 8 bytes | `inode_count` | Total number of pre-allocated inodes |
| 0x20 | 8 bytes | `free_inodes` | Number of currently free inodes |
| 0x28 | 8 bytes | `bitmap_start` | Starting block for the free space bitmap |
| 0x30 | 8 bytes | `inode_table_start` | Starting block for the inode array |
| 0x38 | 8 bytes | `root_inode` | Inode number of the root directory |
| 0x40 | 3904 bytes| `padding` | Reserved for future expansions |

## Alignment
The superblock spans exactly 4096 bytes (1 Block).
