//! I/O operation descriptors and completions.
//!
//! A single vocabulary for every device operation the engine can issue,
//! shared by all backends so the transaction layer can build batches
//! without knowing which plane will execute them. The descriptor is
//! deliberately plain data (48 bytes) so batches are POD arrays the
//! io_uring backend can turn into SQEs with no allocation.

use std::fmt;

/// What the device (or engine) should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    /// Positioned read into the op's buffer.
    Read,
    /// Positioned write from the op's buffer.
    Write,
    /// Write with Force-Unit-Access semantics: the data must be on
    /// non-volatile media when the completion arrives (RFC-002 §5.1
    /// step 4: FUA on the final ring submission of a transaction).
    WriteFua,
    /// Data-only flush of everything previously submitted to this device
    /// (the `fdatasync` of the commit ordering).
    FlushData,
    /// ZNS zone append: the device places the write at the zone's current
    /// write pointer and *returns* the chosen offset in the completion;
    /// the extent record is written at completion time (RFC-002 §6.1).
    ZoneAppend,
    /// Deallocate / trim the range (zone reset for ZNS).
    Deallocate,
}

/// A single device operation. Buffer ownership is expressed as a
/// [`crate::io_engine::zero_copy::BufHandle`] slice range so the same
/// descriptor works for arena-registered zero-copy buffers (io_uring) and
/// plain heap vectors (threaded backend): `buf` is an index + range into
/// whichever arena the engine owns, resolved at submission time.
#[derive(Clone)]
pub struct IoOp {
    pub kind: OpKind,
    /// Pool member index (the storage plane fans out to devices).
    pub device: u16,
    /// Byte offset into the device. For `ZoneAppend` this is the zone
    /// start; the completion carries the final placed offset.
    pub offset: u64,
    /// Payload length in bytes.
    pub len: u32,
    /// Zone id for `ZoneAppend` (write-pointer token selector).
    pub zone: u32,
    /// Buffer arena slot (zero-copy) -- see `zero_copy::BufHandle`.
    pub buf: u32,
    /// Offset within the buffer slot.
    pub buf_off: u32,
    /// Caller's tag, echoed in the completion verbatim (transaction id,
    /// checksum-tree slot, etc.).
    pub user_data: u64,
}

impl IoOp {
    #[must_use]
    pub fn read(
        device: u16,
        offset: u64,
        len: u32,
        buf: u32,
        buf_off: u32,
        user_data: u64,
    ) -> Self {
        Self {
            kind: OpKind::Read,
            device,
            offset,
            len,
            zone: 0,
            buf,
            buf_off,
            user_data,
        }
    }

    #[must_use]
    pub fn write(
        device: u16,
        offset: u64,
        len: u32,
        buf: u32,
        buf_off: u32,
        user_data: u64,
    ) -> Self {
        Self {
            kind: OpKind::Write,
            device,
            offset,
            len,
            zone: 0,
            buf,
            buf_off,
            user_data,
        }
    }

    #[must_use]
    pub fn write_fua(
        device: u16,
        offset: u64,
        len: u32,
        buf: u32,
        buf_off: u32,
        user_data: u64,
    ) -> Self {
        Self {
            kind: OpKind::WriteFua,
            device,
            offset,
            len,
            zone: 0,
            buf,
            buf_off,
            user_data,
        }
    }

    #[must_use]
    pub fn zone_append(
        device: u16,
        zone: u32,
        len: u32,
        buf: u32,
        buf_off: u32,
        user_data: u64,
    ) -> Self {
        Self {
            kind: OpKind::ZoneAppend,
            device,
            offset: 0,
            len,
            zone,
            buf,
            buf_off,
            user_data,
        }
    }

    #[must_use]
    pub fn flush_data(device: u16, user_data: u64) -> Self {
        Self {
            kind: OpKind::FlushData,
            device,
            offset: 0,
            len: 0,
            zone: 0,
            buf: 0,
            buf_off: 0,
            user_data,
        }
    }
}

/// Result of one completed operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpResult {
    /// Bytes read/written, or the placed byte offset for `ZoneAppend`.
    Done(u32),
    /// Flush completed.
    Flushed,
    /// The op failed; the i32 is the engine's errno (PAL errno space).
    Failed(i32),
}

impl OpResult {
    #[must_use]
    pub fn is_err(&self) -> bool {
        matches!(self, OpResult::Failed(_))
    }
}

/// Completion record reaped from the completion queue.
#[derive(Debug, Clone, Copy)]
pub struct Completion {
    /// Echoes `IoOp::user_data`.
    pub user_data: u64,
    /// Echoes the op kind (completions are consumed out of order).
    pub kind: OpKind,
    /// For `ZoneAppend`: the byte offset the device placed the write at.
    pub placed_offset: u64,
    pub result: OpResult,
}

impl Completion {
    #[must_use]
    pub fn ok(user_data: u64, kind: OpKind) -> Self {
        Self {
            user_data,
            kind,
            placed_offset: 0,
            result: OpResult::Flushed,
        }
    }

    #[must_use]
    pub fn data(user_data: u64, kind: OpKind, bytes: u32) -> Self {
        Self {
            user_data,
            kind,
            placed_offset: 0,
            result: OpResult::Done(bytes),
        }
    }

    #[must_use]
    pub fn zone_append(user_data: u64, placed_offset: u64, bytes: u32) -> Self {
        Self {
            user_data,
            kind: OpKind::ZoneAppend,
            placed_offset,
            result: OpResult::Done(bytes),
        }
    }

    #[must_use]
    pub fn err(user_data: u64, kind: OpKind, errno: i32) -> Self {
        Self {
            user_data,
            kind,
            placed_offset: 0,
            result: OpResult::Failed(errno),
        }
    }
}

impl fmt::Display for OpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            OpKind::Read => "read",
            OpKind::Write => "write",
            OpKind::WriteFua => "write-fua",
            OpKind::FlushData => "flush",
            OpKind::ZoneAppend => "zone-append",
            OpKind::Deallocate => "deallocate",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_are_plain_data() {
        let ops = [
            IoOp::read(0, 4096, 4096, 1, 0, 42),
            IoOp::write_fua(0, 8192, 4096, 1, 4096, 43),
            IoOp::zone_append(0, 7, 16 * 1024, 2, 0, 44),
            IoOp::flush_data(0, 45),
        ];
        assert_eq!(ops[0].kind, OpKind::Read);
        assert_eq!(ops[1].kind, OpKind::WriteFua);
        assert_eq!(ops[2].zone, 7);
        assert_eq!(ops[3].user_data, 45);
        // 48-byte POD: fits in a cache line with room for one neighbor.
        assert!(std::mem::size_of::<IoOp>() <= 48);
    }

    #[test]
    fn completion_shapes() {
        let c = Completion::zone_append(9, 0x1_0000, 16 * 1024);
        assert_eq!(c.placed_offset, 0x1_0000);
        assert_eq!(c.result, OpResult::Done(16 * 1024));
        let e = Completion::err(1, OpKind::Read, 5);
        assert!(e.result.is_err());
    }
}
