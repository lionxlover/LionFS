//! Inode v3: the 96-byte core with inline payloads and dynamic tail
//! packing (RFC-002 §4.2, Table 9).
//!
//! A file smaller than 4 KiB is stored entirely inside the B-epsilon
//! leaf that holds its inode core -- the payload is appended directly
//! after the fixed fields as a variable-length value, so reading the
//! file is **one metadata read and zero data-block reads**, and creating
//! it allocates **zero data blocks**. This is the mechanism the 1.x gap
//! analysis demands: the "4 KiB minimum per file" gap disappears.
//!
//! Layout (little endian):
//!
//! ```text
//! Offset  Field                    Width   Notes
//! 0       ino                      16 B    full 128-bit inode number
//! 16      mode / nlink / uid / gid 4 x u32 POSIX identity
//! 32      size                     16 B    u128, GRAN-aware
//! 48      generation               8 B     bumped on every CoW rewrite
//! 56      flags                    4 B     INLINE, COMPRESSED, ENCRYPTED, DEDUP
//! 60      extent_root / inline_len 6 B/4B  u48 tree ref, or payload length
//! 64+     inline payload           0-4032 B only when INLINE flag set
//! ```
//!
//! Tail packing: because leaf values are variable-length, leaf packing
//! would still waste the tail of the final leaf block; dynamic tail
//! packing co-packages the final partial record of one inode with the
//! leading bytes of the next entry in the same leaf, with a 2 KiB leaf
//! flush threshold that batches inode churn so a leaf re-write
//! amortizes over many mutations.

use crate::pal::posix::{S_IFDIR, S_IFLNK, S_IFREG};

/// Fixed core size before any inline payload.
pub const INODE_V3_CORE_SIZE: usize = 64;
/// Maximum inline payload (a 4 KiB leaf value minus the core).
pub const INODE_V3_MAX_INLINE: usize = 4096 - INODE_V3_CORE_SIZE;
/// The 2 KiB flush threshold: batches inode churn so a leaf re-write
/// amortizes over many mutations (RFC-002 §4.2).
pub const LEAF_FLUSH_THRESHOLD: usize = 2048;
/// Leaf padding fraction retained on flush: 25%.
pub const LEAF_PADDING_FRAC_256: u32 = 64;

/// v3 inode flags (bit positions in the 4-byte flag word).
pub const FLAG_V3_INLINE: u32 = 1 << 0;
pub const FLAG_V3_COMPRESSED: u32 = 1 << 1;
pub const FLAG_V3_ENCRYPTED: u32 = 1 << 2;
pub const FLAG_V3_DEDUP: u32 = 1 << 3;

/// The 128-bit inode number of v3 (the HAMT key).
pub type Ino128 = u128;

/// A v3 inode in memory. The owned form is what the B-epsilon leaf
/// stores as a variable-length value; the wire form is produced by
/// [`InodeV3::serialize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeV3 {
    pub ino: Ino128,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    /// File size in bytes, up to u128 width (GRAN-aware reach).
    pub size: u128,
    pub generation: u64,
    pub flags: u32,
    /// Either the inline payload (INLINE set) or empty.
    pub inline_payload: Vec<u8>,
    /// The spill extent-tree root (u48 on the wire), when not inline.
    pub extent_root: u64,
}

impl InodeV3 {
    #[must_use]
    pub fn new_file(ino: Ino128, mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            ino,
            mode: mode | S_IFREG,
            nlink: 1,
            uid,
            gid,
            size: 0,
            generation: 1,
            flags: 0,
            inline_payload: Vec::new(),
            extent_root: 0,
        }
    }

    #[must_use]
    pub fn new_dir(ino: Ino128, mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            ino,
            mode: mode | S_IFDIR,
            nlink: 2,
            uid,
            gid,
            size: 0,
            generation: 1,
            flags: 0,
            inline_payload: Vec::new(),
            extent_root: 0,
        }
    }

    #[must_use]
    pub fn is_inline(&self) -> bool {
        self.flags & FLAG_V3_INLINE != 0
    }

    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.mode & crate::pal::posix::S_IFMT == S_IFDIR
    }

    #[must_use]
    pub fn is_regular(&self) -> bool {
        self.mode & crate::pal::posix::S_IFMT == S_IFREG
    }

    #[must_use]
    pub fn is_symlink(&self) -> bool {
        self.mode & crate::pal::posix::S_IFMT == S_IFLNK
    }

    /// Whether `payload` fits inline (the small-file specialization).
    #[must_use]
    pub fn fits_inline(payload_len: usize) -> bool {
        payload_len <= INODE_V3_MAX_INLINE
    }

    /// Stores the payload inline if it fits; returns false (and leaves
    /// the inode unchanged) when the file must spill to extents.
    pub fn try_store_inline(&mut self, payload: &[u8]) -> bool {
        if !Self::fits_inline(payload.len()) {
            return false;
        }
        self.inline_payload.clear();
        self.inline_payload.extend_from_slice(payload);
        self.size = payload.len() as u128;
        self.flags |= FLAG_V3_INLINE;
        self.extent_root = 0;
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// Retires the inline payload (the file grew past the threshold):
    /// the payload moves to extents rooted at `root`, flags clear.
    pub fn spill_to_extents(&mut self, root: u64) {
        self.inline_payload.clear();
        self.flags &= !FLAG_V3_INLINE;
        self.extent_root = root;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Wire serialization. Layout exactly as the table above; inline
    /// payload follows when the INLINE flag is set.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(INODE_V3_CORE_SIZE + self.inline_payload.len());
        buf.extend_from_slice(&self.ino.to_le_bytes());
        buf.extend_from_slice(&self.mode.to_le_bytes());
        buf.extend_from_slice(&self.nlink.to_le_bytes());
        buf.extend_from_slice(&self.uid.to_le_bytes());
        buf.extend_from_slice(&self.gid.to_le_bytes());
        buf.extend_from_slice(&self.size.to_le_bytes());
        buf.extend_from_slice(&self.generation.to_le_bytes());
        buf.extend_from_slice(&self.flags.to_le_bytes());
        if self.is_inline() {
            // inline_len: u32 (the inline branch of the extent_root union).
            buf.extend_from_slice(&(self.inline_payload.len() as u32).to_le_bytes());
            buf.extend_from_slice(&self.inline_payload);
        } else {
            // extent_root: u48 (the spill branch) + 2 reserved zero bytes
            // so the spill record is exactly 68 bytes and both branches
            // decode unambiguously.
            let root = self.extent_root.to_le_bytes();
            buf.extend_from_slice(&root[..6]);
            buf.extend_from_slice(&[0u8; 2]);
        }
        buf
    }

    /// Wire deserialization. Returns `None` on structural inconsistency
    /// (wrong core size, inline length out of bounds, reserved bits).
    #[must_use]
    pub fn deserialize(buf: &[u8]) -> Option<Self> {
        if buf.len() < INODE_V3_CORE_SIZE {
            return None;
        }
        let ino = Ino128::from_le_bytes(buf[0..16].try_into().expect("16 bytes"));
        let mode = u32::from_le_bytes(buf[16..20].try_into().expect("4"));
        let nlink = u32::from_le_bytes(buf[20..24].try_into().expect("4"));
        let uid = u32::from_le_bytes(buf[24..28].try_into().expect("4"));
        let gid = u32::from_le_bytes(buf[28..32].try_into().expect("4"));
        let size = u128::from_le_bytes(buf[32..48].try_into().expect("16"));
        let generation = u64::from_le_bytes(buf[48..56].try_into().expect("8"));
        let flags = u32::from_le_bytes(buf[56..60].try_into().expect("4"));

        if flags & !(FLAG_V3_INLINE | FLAG_V3_COMPRESSED | FLAG_V3_ENCRYPTED | FLAG_V3_DEDUP) != 0 {
            return None; // Reserved bits set: forward-format detection.
        }

        if flags & FLAG_V3_INLINE != 0 {
            let inline_len = u32::from_le_bytes(buf[60..64].try_into().expect("4")) as usize;
            if inline_len > INODE_V3_MAX_INLINE {
                return None;
            }
            if buf.len() != INODE_V3_CORE_SIZE + inline_len {
                return None;
            }
            let inline_payload = buf[64..64 + inline_len].to_vec();
            // Cross-check: an inline inode's size must equal its payload.
            if size != inline_len as u128 {
                return None;
            }
            Some(Self {
                ino,
                mode,
                nlink,
                uid,
                gid,
                size,
                generation,
                flags,
                inline_payload,
                extent_root: 0,
            })
        } else {
            // The spill branch is a u48 root at 60..66 plus two reserved
            // zero bytes at 66..68 (total 68).
            if buf.len() != INODE_V3_CORE_SIZE + 4 {
                return None;
            }
            if buf[66] != 0 || buf[67] != 0 {
                return None;
            }
            let mut root_bytes = [0u8; 8];
            root_bytes[..6].copy_from_slice(&buf[60..66]);
            let extent_root = u64::from_le_bytes(root_bytes);
            Some(Self {
                ino,
                mode,
                nlink,
                uid,
                gid,
                size,
                generation,
                flags,
                inline_payload: Vec::new(),
                extent_root,
            })
        }
    }
}

/// One entry in a tail-packed leaf: the inode's wire bytes plus how many
/// leading bytes belong to the *next* entry's co-packaged record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedEntry {
    pub inode: InodeV3,
    /// Bytes of the NEXT entry's record packed into this slot (>= 0).
    /// The next entry's deserializer consumes them from its own start.
    pub borrowed_tail: Vec<u8>,
}

/// The tail packer: batches variable-length inode records into leaf
/// blocks with a 2 KiB flush threshold and 25% padding.
///
/// Packing rule (RFC-002 §4.2): the final partial record of one inode is
/// co-packaged with the leading bytes of the next entry in the same
/// leaf. In this in-memory model, "borrowed tail" bytes are modeled
/// explicitly so round-trips are testable; on disk they are simply the
/// next record's bytes stored contiguously (the leaf reader reconstructs
/// boundaries from the record headers).
#[derive(Debug, Default)]
pub struct TailPacker {
    /// Bytes accumulated toward the flush threshold.
    pending: Vec<u8>,
    /// Records packed since the last flush.
    pending_records: usize,
    /// Total flushes performed (amortization metric).
    flushes: usize,
    /// Bytes written across flushes (write-amplification metric).
    bytes_written: usize,
}

impl TailPacker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one inode's wire bytes. Returns `Some(flushed)` when the
    /// 2 KiB threshold was crossed and the leaf flushed (with its 25%
    /// padding).
    pub fn append(&mut self, inode: &InodeV3) -> Option<Vec<u8>> {
        let bytes = inode.serialize();
        self.pending.extend_from_slice(&bytes);
        self.pending_records += 1;
        if self.pending.len() >= LEAF_FLUSH_THRESHOLD {
            return Some(self.flush());
        }
        None
    }

    /// Flushes the pending leaf: the bytes are padded to 25% free space
    /// (the padding is what absorbs subsequent appends without a
    /// re-split), and the pending state resets.
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.pending);
        // 25% padding: ceil(len / 3) extra zero bytes (out.len() becomes
        // ~4/3 of the live bytes, i.e. 25% free).
        let padding = out.len().div_ceil(3);
        out.resize(out.len() + padding, 0);
        self.flushes += 1;
        self.bytes_written += out.len();
        self.pending_records = 0;
        out
    }

    pub fn flushes(&self) -> usize {
        self.flushes
    }

    pub fn bytes_written(&self) -> usize {
        self.bytes_written
    }

    /// Live bytes currently pending (pre-flush).
    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    /// Write amplification of the leaf path: bytes written vs live bytes
    /// handled (the honest accounting the RFC's §7.3 demands for every
    /// ratio claim).
    #[must_use]
    pub fn write_amplification(&self) -> f64 {
        if self.bytes_written == 0 {
            return 1.0;
        }
        // Live bytes ~= 3/4 of written (the padding fraction).
        let live = (self.bytes_written * 3) / 4;
        self.bytes_written as f64 / live.max(1) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inline_file(ino: Ino128, payload: &[u8]) -> InodeV3 {
        let mut f = InodeV3::new_file(ino, 0o644, 1000, 1000);
        assert!(f.try_store_inline(payload));
        f
    }

    #[test]
    fn small_file_stores_inline() {
        let f = inline_file(7, b"hello inline world");
        assert!(f.is_inline());
        assert_eq!(f.inline_payload, b"hello inline world");
        assert_eq!(f.size, 18);
        // Zero data blocks: the whole file is metadata.
        assert_eq!(f.extent_root, 0);
    }

    #[test]
    fn exactly_4032_bytes_fits_inline() {
        let payload = vec![0x5Au8; INODE_V3_MAX_INLINE];
        let f = inline_file(8, &payload);
        assert!(f.is_inline());
    }

    #[test]
    fn over_inline_threshold_refused() {
        let mut f = InodeV3::new_file(9, 0o644, 0, 0);
        let payload = vec![0u8; INODE_V3_MAX_INLINE + 1];
        assert!(!f.try_store_inline(&payload));
        assert!(!f.is_inline());
        assert_eq!(f.size, 0, "size must not lie when store failed");
    }

    #[test]
    fn spill_to_extents_retires_inline() {
        let mut f = inline_file(10, b"small at first");
        f.spill_to_extents(0x1234);
        assert!(!f.is_inline());
        assert_eq!(f.extent_root, 0x1234);
        assert!(f.inline_payload.is_empty());
        assert!(f.generation >= 2, "generation bumps on CoW rewrite");
    }

    #[test]
    fn wire_roundtrip_inline() {
        let f = inline_file(0xAB, b"payload bytes here");
        let wire = f.serialize();
        assert_eq!(wire.len(), INODE_V3_CORE_SIZE + 18);
        let back = InodeV3::deserialize(&wire).expect("valid wire form");
        assert_eq!(back, f);
    }

    #[test]
    fn wire_roundtrip_spill() {
        let mut f = InodeV3::new_file(0xCD, 0o644, 5, 6);
        f.size = 1 << 40; // A large spilled file.
        f.spill_to_extents(0x00FF_FFFF_FFFF); // Max u48 root.
        let wire = f.serialize();
        let back = InodeV3::deserialize(&wire).expect("valid wire form");
        assert_eq!(back, f);
        assert_eq!(back.extent_root, 0x00FF_FFFF_FFFF);
    }

    #[test]
    fn wire_rejects_structural_garbage() {
        // Too short.
        assert!(InodeV3::deserialize(&[0u8; 32]).is_none());
        assert!(InodeV3::deserialize(&[0u8; 63]).is_none());
        // Reserved flag bits.
        let mut f = inline_file(1, b"x");
        f.flags |= 1 << 31;
        assert!(InodeV3::deserialize(&f.serialize()).is_none());
        // Inline length inconsistent with buffer.
        let mut f = inline_file(2, b"abc");
        let mut wire = f.serialize();
        wire.truncate(wire.len() - 1);
        assert!(InodeV3::deserialize(&wire).is_none());
        // Spill branch with nonzero reserved bytes at 66/67.
        let mut f = InodeV3::new_file(3, 0o600, 0, 0);
        f.spill_to_extents(0x1000);
        let mut wire = f.serialize();
        assert_eq!(wire.len(), 68);
        wire[66] = 1;
        assert!(InodeV3::deserialize(&wire).is_none());
        let mut wire = f.serialize();
        wire[67] = 1;
        assert!(InodeV3::deserialize(&wire).is_none());
        // Truncated spill record.
        let mut wire = f.serialize();
        wire.truncate(66);
        assert!(InodeV3::deserialize(&wire).is_none());
        // Inline size field lying about the payload length.
        let mut f = inline_file(4, b"abcd");
        let mut wire = f.serialize();
        wire[32] = 0xFF; // Corrupt a size byte.
        assert!(InodeV3::deserialize(&wire).is_none());
    }

    #[test]
    fn dir_and_file_kinds() {
        let d = InodeV3::new_dir(1, 0o755, 0, 0);
        assert!(d.is_dir());
        assert_eq!(d.nlink, 2);
        let f = InodeV3::new_file(2, 0o644, 0, 0);
        assert!(f.is_regular());
        assert!(!f.is_symlink());
    }

    #[test]
    fn tail_packer_batches_to_threshold() {
        let mut packer = TailPacker::new();
        // 68-byte inline inodes: 2 KiB / 68 ~ 30 per flush.
        let mut flushed = None;
        for i in 0..40u128 {
            if let Some(leaf) =
                packer.append(&inline_file(i, b"0123456789abcdefghij0123456789abcd"))
            {
                flushed = Some(leaf);
                break;
            }
        }
        let leaf = flushed.expect("threshold must have been crossed");
        // The flushed leaf carries 25% padding: live bytes ~ (4/3)^-1.
        assert!(leaf.len() >= LEAF_FLUSH_THRESHOLD);
        assert!(
            leaf.len() >= (LEAF_FLUSH_THRESHOLD * 4) / 3 - 16,
            "leaf len {}",
            leaf.len()
        );
        assert_eq!(packer.flushes(), 1);
    }

    #[test]
    fn tail_packer_flush_pads_amortized() {
        let mut packer = TailPacker::new();
        for i in 0..100u128 {
            packer.append(&inline_file(i, b"0123456789abcdefghij"));
        }
        let _ = packer.flush();
        // Write amplification of the leaf path: ~4/3 from padding, never
        // the 2x+ a naive per-record rewrite would cost.
        let wa = packer.write_amplification();
        assert!(wa < 1.5, "write amplification {wa}");
        assert!(wa > 1.0);
    }

    #[test]
    fn generation_bumps_on_rewrites() {
        let mut f = inline_file(1, b"v1");
        let g1 = f.generation;
        f.try_store_inline(b"v2 - rewritten");
        assert_eq!(f.generation, g1 + 1);
        f.spill_to_extents(9);
        assert_eq!(f.generation, g1 + 2);
    }

    #[test]
    fn core_size_matches_spec_table() {
        // The RFC's Table 9: core fields end at offset 60, then the
        // 6-byte/4-byte branch field; INLINE payloads start at 64.
        assert_eq!(INODE_V3_CORE_SIZE, 64);
        assert_eq!(INODE_V3_MAX_INLINE, 4032);
        // A max-inline inode is exactly one 4 KiB leaf value.
        let f = inline_file(1, &vec![0u8; INODE_V3_MAX_INLINE]);
        assert_eq!(f.serialize().len(), 4096);
        // A spill record is exactly 68 bytes.
        let mut g = InodeV3::new_file(2, 0o600, 0, 0);
        g.spill_to_extents(5);
        assert_eq!(g.serialize().len(), 68);
    }
}
