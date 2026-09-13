//! 3.6 Extended-attribute on-disk block format ("LXAT").
//!
//! One self-describing 4 KiB block per inode that carries xattrs,
//! referenced from the `XattrTree` (node type 13) record for that
//! inode. This mirrors the ext4 shape (one xattr block per inode) but
//! is fully self-describing: magic + version + fletcher32 over the
//! entry region, variable-length TLV entries -- so a future reader
//! with nothing but this module can decode every attribute on a
//! 200-year-old image (the Format Vault "self-describing format"
//! requirement; see `docs/rfc/LFS-RFC-005-format-vault.md`).
//!
//! Wire layout (all little-endian):
//!
//! ```text
//! [ 0.. 4)  magic "LXAT"
//! [ 4.. 6)  version (1)
//! [ 6.. 8)  entry_count
//! [ 8..12)  used       -- bytes of the entry region in use
//! [12..20)  next_block -- overflow chain root (0 = none; reserved for
//!                        future large-value spill, unused in 3.6)
//! [20..24)  fletcher32 over the entry region
//! [24..32)  reserved (zeros)
//! [32..4096) entries, packed back to back
//! ```
//!
//! Each entry:
//!
//! ```text
//! [0]     name_len (1..=255)
//! [1]     ns_flags (namespace class; informational, derived from the
//!         name prefix -- the full name INCLUDING its prefix is stored)
//! [2..4)  value_len (u16 LE)
//! [4..)   name bytes, then value bytes
//! ```
//!
//! Capacity: 4096 - 32 = 4064 bytes of entry region per inode. A
//! setxattr that does not fit returns `ENOSPC` (the ext4-class
//! behavior for block-based xattr storage; the kernel already caps
//! `user.*` values at one block for most filesystems).

use crate::common::checksum::fletcher32;
use std::io::{Error, ErrorKind, Result};

pub const XATTR_BLOCK_MAGIC: u32 = 0x5441_584C; // "LXAT" (little-endian)
pub const XATTR_BLOCK_VERSION: u16 = 1;
pub const XATTR_HEADER_SIZE: usize = 32;
pub const XATTR_ENTRY_OVERHEAD: usize = 4;
/// Maximum bytes of entry region available in one xattr block.
pub const XATTR_CAPACITY: usize = crate::ondisk::serialization::BLOCK_SIZE - XATTR_HEADER_SIZE;
/// Longest xattr name (Linux `XATTR_NAME_MAX` convention).
pub const XATTR_NAME_MAX: usize = 255;

// Namespace classes (informational; the full name including its
// prefix is what is stored and matched).
pub const NS_USER: u8 = 0;
pub const NS_SECURITY: u8 = 1;
pub const NS_TRUSTED: u8 = 2;
pub const NS_SYSTEM: u8 = 3;

/// Linux setxattr flags (FUSE forwards them verbatim).
pub const XATTR_CREATE: i32 = 0x1;
pub const XATTR_REPLACE: i32 = 0x2;

/// One decoded extended attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrEntry {
    /// Full attribute name including its namespace prefix
    /// (`user.mime_type`, `system.posix_acl_access`, ...).
    pub name: String,
    pub value: Vec<u8>,
    pub ns_flags: u8,
}

/// Classify + validate an xattr name. Returns the namespace class.
/// Rules (Linux xattr(7) shape):
/// * the name must be 1..=255 bytes,
/// * it must carry a known namespace prefix (`user.`, `security.`,
///   `trusted.`, `system.`),
/// * everything after the prefix must be non-empty.
pub fn classify_name(name: &str) -> Result<u8> {
    if name.is_empty() || name.len() > XATTR_NAME_MAX {
        return Err(Error::new(ErrorKind::InvalidInput, "xattr name length out of range"));
    }
    if let Some(rest) = name.strip_prefix("user.") {
        require_body(rest, name, NS_USER)?;
        return Ok(NS_USER);
    }
    if let Some(rest) = name.strip_prefix("security.") {
        require_body(rest, name, NS_SECURITY)?;
        return Ok(NS_SECURITY);
    }
    if let Some(rest) = name.strip_prefix("trusted.") {
        require_body(rest, name, NS_TRUSTED)?;
        return Ok(NS_TRUSTED);
    }
    if let Some(rest) = name.strip_prefix("system.") {
        require_body(rest, name, NS_SYSTEM)?;
        return Ok(NS_SYSTEM);
    }
    Err(Error::new(
        ErrorKind::InvalidInput,
        "xattr name must start with user. / security. / trusted. / system.",
    ))
}

fn require_body(rest: &str, _full: &str, _ns: u8) -> Result<()> {
    if rest.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "xattr name body is empty"));
    }
    Ok(())
}

#[allow(dead_code)]
fn ns_class_of(name: &str) -> u8 {
    classify_name(name).unwrap_or(NS_USER)
}

/// Parse a 4 KiB xattr block into its entries. Corruption (bad magic,
/// bad version, checksum mismatch, truncated entry) is `InvalidData`.
pub fn parse_block(buf: &[u8]) -> Result<Vec<XattrEntry>> {
    if buf.len() < XATTR_HEADER_SIZE {
        return Err(Error::new(ErrorKind::InvalidData, "xattr block shorter than header"));
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != XATTR_BLOCK_MAGIC {
        return Err(Error::new(ErrorKind::InvalidData, "xattr block bad magic"));
    }
    let version = u16::from_le_bytes([buf[4], buf[5]]);
    if version != XATTR_BLOCK_VERSION {
        return Err(Error::new(ErrorKind::InvalidData, "xattr block bad version"));
    }
    let entry_count = u16::from_le_bytes([buf[6], buf[7]]) as usize;
    let used = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    let stored_csum = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    if used > XATTR_CAPACITY {
        return Err(Error::new(ErrorKind::InvalidData, "xattr block used field overflows"));
    }
    let entries_region = &buf[XATTR_HEADER_SIZE..XATTR_HEADER_SIZE + used];
    if fletcher32(entries_region) != stored_csum {
        return Err(Error::new(ErrorKind::InvalidData, "xattr block checksum mismatch"));
    }

    let mut entries = Vec::with_capacity(entry_count);
    let mut pos = 0usize;
    for _ in 0..entry_count {
        if pos + XATTR_ENTRY_OVERHEAD > entries_region.len() {
            return Err(Error::new(ErrorKind::InvalidData, "xattr entry header truncated"));
        }
        let name_len = entries_region[pos] as usize;
        let ns_flags = entries_region[pos + 1];
        let value_len = u16::from_le_bytes([entries_region[pos + 2], entries_region[pos + 3]]) as usize;
        pos += XATTR_ENTRY_OVERHEAD;
        let total = name_len + value_len;
        if name_len == 0 || pos + total > entries_region.len() {
            return Err(Error::new(ErrorKind::InvalidData, "xattr entry truncated"));
        }
        let name = String::from_utf8_lossy(&entries_region[pos..pos + name_len]).into_owned();
        pos += name_len;
        let value = entries_region[pos..pos + value_len].to_vec();
        pos += value_len;
        entries.push(XattrEntry { name, value, ns_flags });
    }
    Ok(entries)
}

/// Serialize entries into a fresh 4 KiB block. `ENOSPC` when the
/// entries do not fit (the caller maps that to the POSIX ENOSPC).
pub fn write_block(entries: &[XattrEntry]) -> Result<[u8; crate::ondisk::serialization::BLOCK_SIZE]> {
    let mut region_len = 0usize;
    for e in entries {
        if e.name.is_empty() || e.name.len() > XATTR_NAME_MAX {
            return Err(Error::new(ErrorKind::InvalidInput, "xattr name length out of range"));
        }
        if e.value.len() > u16::MAX as usize {
            return Err(Error::new(ErrorKind::InvalidInput, "xattr value too large"));
        }
        region_len += XATTR_ENTRY_OVERHEAD + e.name.len() + e.value.len();
    }
    if region_len > XATTR_CAPACITY {
        return Err(Error::new(ErrorKind::StorageFull, "xattr block capacity exceeded"));
    }

    let mut buf = [0u8; crate::ondisk::serialization::BLOCK_SIZE];
    buf[0..4].copy_from_slice(&XATTR_BLOCK_MAGIC.to_le_bytes());
    buf[4..6].copy_from_slice(&XATTR_BLOCK_VERSION.to_le_bytes());
    buf[6..8].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    buf[8..12].copy_from_slice(&(region_len as u32).to_le_bytes());
    // next_block (12..20) stays zero: no overflow chain in 3.6.
    let mut pos = XATTR_HEADER_SIZE;
    for e in entries {
        buf[pos] = e.name.len() as u8;
        buf[pos + 1] = e.ns_flags;
        buf[pos + 2..pos + 4].copy_from_slice(&(e.value.len() as u16).to_le_bytes());
        pos += XATTR_ENTRY_OVERHEAD;
        buf[pos..pos + e.name.len()].copy_from_slice(e.name.as_bytes());
        pos += e.name.len();
        buf[pos..pos + e.value.len()].copy_from_slice(&e.value);
        pos += e.value.len();
    }
    let csum = fletcher32(&buf[XATTR_HEADER_SIZE..XATTR_HEADER_SIZE + region_len]);
    buf[20..24].copy_from_slice(&csum.to_le_bytes());
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, value: &[u8]) -> XattrEntry {
        XattrEntry { name: name.to_string(), value: value.to_vec(), ns_flags: ns_class_of(name) }
    }

    #[test]
    fn roundtrips_entries() {
        let entries = vec![
            entry("user.mime_type", b"text/plain"),
            entry("system.posix_acl_access", &[1, 2, 3, 4, 5, 6, 7, 8]),
            entry("trusted.origin", b"lionfs"),
        ];
        let buf = write_block(&entries).expect("fits");
        let parsed = parse_block(&buf).expect("parses");
        assert_eq!(parsed, entries);
    }

    #[test]
    fn empty_block_roundtrips() {
        let buf = write_block(&[]).expect("fits");
        let parsed = parse_block(&buf).expect("parses");
        assert!(parsed.is_empty());
    }

    #[test]
    fn detects_corruption() {
        let entries = vec![entry("user.a", b"hello")];
        let mut buf = write_block(&entries).expect("fits");
        // Flip a byte INSIDE the checksummed entry region (the header
        // is 32 bytes; the first entry's value bytes start at 32+4+6).
        buf[45] ^= 0xFF;
        assert!(parse_block(&buf).is_err());
    }

    #[test]
    fn rejects_oversize() {
        let big = vec![b'x'; XATTR_CAPACITY];
        let entries = vec![entry("user.big", &big)];
        assert!(write_block(&entries).is_err());
    }

    #[test]
    fn name_classification() {
        assert_eq!(classify_name("user.a").unwrap(), NS_USER);
        assert_eq!(classify_name("system.posix_acl_access").unwrap(), NS_SYSTEM);
        assert_eq!(classify_name("security.selinux").unwrap(), NS_SECURITY);
        assert_eq!(classify_name("trusted.x").unwrap(), NS_TRUSTED);
        assert!(classify_name("noclass.x").is_err());
        assert!(classify_name("user.").is_err());
        assert!(classify_name("").is_err());
        let long = format!("user.{}", "a".repeat(300));
        assert!(classify_name(&long).is_err());
    }
}
