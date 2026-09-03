# Directory Specification

Directories in LionFS are essentially standard files (`mode` = `S_IFDIR`) containing a sequential list of `DirEntryHeader` structures followed immediately by variable-length UTF-8 file names.

## DirEntryHeader Structure (Little Endian)

| Offset | Size | Name | Description |
|---|---|---|---|
| 0x00 | 8 bytes | `ino` | Target Inode number (0 = free slot) |
| 0x08 | 2 bytes | `rec_len` | Total byte length of this record (Header + Name + Padding) |
| 0x0A | 1 byte | `name_len` | Actual byte length of the name string |
| 0x0B | 1 byte | `file_type` | Type identifier (1 = Reg, 2 = Dir) |
| 0x0C | 4 bytes | `padding` | 4-byte padding for 8-byte alignment |

Total Size: 16 Bytes.

Records are padded to ensure the next `DirEntryHeader` aligns naturally.
