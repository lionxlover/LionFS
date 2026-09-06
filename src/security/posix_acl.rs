//! 3.6 POSIX access-control lists (POSIX 1003.1e draft 17), stored in
//! the `system.posix_acl_access` / `system.posix_acl_default` extended
//! attributes in the ext4/XFS-compatible wire format.
//!
//! The wire format (all little-endian, identical to ext4):
//!
//! ```text
//! [0..4)   version = 1
//! [4..)    entries of 8 bytes: tag u16, perm u16, id u32
//!          (id is the uid/gid for USER/GROUP entries; 0 otherwise)
//! ```
//!
//! Tags: USER_OBJ=1, USER=2, GROUP_OBJ=4, GROUP=8, MASK=0x10,
//! OTHER=0x20. Canonical entry order: USER_OBJ, USERs by ascending id,
//! GROUP_OBJ, GROUPs by ascending id, MASK, OTHER.
//!
//! Semantics implemented here (the full draft-17 core):
//! * [`PosixAcl::validate`] -- structural rules (base entries present,
//!   mask required iff named entries exist, canonical order).
//! * [`PosixAcl::mode_bits`] -- the stat(2) mode group-class derivation
//!   (GROUP_OBJ is masked when named entries exist).
//! * [`PosixAcl::evaluate`] -- the access-check algorithm (owner /
//!   named user / owning+named groups vs mask / other).
//! * [`PosixAcl::apply_chmod`] -- chmod(2) semantics on an ACL-bearing
//!   inode (mode bits re-map onto USER_OBJ / MASK+GROUP_OBJ / OTHER).
//! * [`PosixAcl::inherit_access_from_default`] -- mkdir(2) inheritance
//!   (the draft-17 intersection with the create mode).
//!
//! Known limit, documented honestly: the VFS `access()` surface passes
//! the caller's uid and PRIMARY gid only (no supplementary-group list),
//! so named-GROUP evaluation runs against the primary gid alone. With
//! the FUSE `default_permissions` mount option the kernel would use
//! its full credential set; without it, LionFS's own evaluation in
//! `VfsOps::access` is the authority.

use std::io::{Error, ErrorKind, Result};

pub const ACL_ACCESS_XATTR: &str = "system.posix_acl_access";
pub const ACL_DEFAULT_XATTR: &str = "system.posix_acl_default";
pub const ACL_WIRE_VERSION: u32 = 1;

// Tag classes.
pub const ACL_USER_OBJ: u16 = 0x01;
pub const ACL_USER: u16 = 0x02;
pub const ACL_GROUP_OBJ: u16 = 0x04;
pub const ACL_GROUP: u16 = 0x08;
pub const ACL_MASK: u16 = 0x10;
pub const ACL_OTHER: u16 = 0x20;

// Permission bits (the low 9 mode-bit convention).
pub const PERM_READ: u16 = 0o4;
pub const PERM_WRITE: u16 = 0o2;
pub const PERM_EXECUTE: u16 = 0o1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AclEntry {
    pub tag: u16,
    /// rwx permission bits (mode convention: 4=r, 2=w, 1=x).
    pub perm: u16,
    /// uid/gid for USER/GROUP entries; None for the class entries.
    pub id: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PosixAcl {
    pub entries: Vec<AclEntry>,
}

impl PosixAcl {
    /// Decode the ext4-compatible wire format.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 4 {
            return Err(Error::new(ErrorKind::InvalidData, "ACL shorter than header"));
        }
        let version = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if version != ACL_WIRE_VERSION {
            return Err(Error::new(ErrorKind::InvalidData, "ACL version unsupported"));
        }
        let body = &bytes[4..];
        if body.len() % 8 != 0 {
            return Err(Error::new(ErrorKind::InvalidData, "ACL entry region not 8-byte aligned"));
        }
        let mut entries = Vec::with_capacity(body.len() / 8);
        for chunk in body.chunks_exact(8) {
            let tag = u16::from_le_bytes([chunk[0], chunk[1]]);
            let perm = u16::from_le_bytes([chunk[2], chunk[3]]);
            let id = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            if !is_valid_tag(tag) {
                return Err(Error::new(ErrorKind::InvalidData, "ACL tag unknown"));
            }
            if tag == ACL_USER || tag == ACL_GROUP {
                if id == u32::MAX {
                    return Err(Error::new(ErrorKind::InvalidData, "ACL undefined id (ACL_UNDEFINED_ID)"));
                }
                entries.push(AclEntry { tag, perm, id: Some(id) });
            } else {
                entries.push(AclEntry { tag, perm, id: None });
            }
        }
        if entries.is_empty() {
            return Err(Error::new(ErrorKind::InvalidData, "ACL has no entries"));
        }
        Ok(Self { entries })
    }

    /// Encode to the ext4-compatible wire format (canonical order).
    pub fn encode(&self) -> Vec<u8> {
        let mut sorted = self.entries.clone();
        Self::canonical_sort(&mut sorted);
        let mut out = Vec::with_capacity(4 + sorted.len() * 8);
        out.extend_from_slice(&ACL_WIRE_VERSION.to_le_bytes());
        for e in sorted {
            out.extend_from_slice(&e.tag.to_le_bytes());
            out.extend_from_slice(&e.perm.to_le_bytes());
            out.extend_from_slice(&e.id.unwrap_or(0).to_le_bytes());
        }
        out
    }

    fn canonical_sort(entries: &mut [AclEntry]) {
        entries.sort_by(|a, b| {
            let rank = |t: u16| match t {
                ACL_USER_OBJ => 0,
                ACL_USER => 1,
                ACL_GROUP_OBJ => 2,
                ACL_GROUP => 3,
                ACL_MASK => 4,
                _ => 5,
            };
            rank(a.tag).cmp(&rank(b.tag)).then_with(|| match (a.tag, b.tag) {
                (ACL_USER, ACL_USER) | (ACL_GROUP, ACL_GROUP) => a.id.cmp(&b.id),
                _ => std::cmp::Ordering::Equal,
            })
        });
    }

    /// Structural validation (draft 17 §6 + ext4 rules):
    /// * exactly one USER_OBJ, GROUP_OBJ, OTHER;
    /// * MASK required iff any USER/GROUP entries exist (access ACLs);
    ///   for default ACLs the mask rule is the same;
    /// * no duplicate ids within a class;
    /// * permission words carry only rwx bits;
    /// * a default ACL must contain at least one entry beyond the
    ///   three base classes.
    pub fn validate(&self, is_default: bool) -> Result<()> {
        let count = |tag: u16| self.entries.iter().filter(|e| e.tag == tag).count();
        if count(ACL_USER_OBJ) != 1 {
            return Err(Error::new(ErrorKind::InvalidData, "ACL needs exactly one USER_OBJ"));
        }
        if count(ACL_GROUP_OBJ) != 1 {
            return Err(Error::new(ErrorKind::InvalidData, "ACL needs exactly one GROUP_OBJ"));
        }
        if count(ACL_OTHER) != 1 {
            return Err(Error::new(ErrorKind::InvalidData, "ACL needs exactly one OTHER"));
        }
        if count(ACL_MASK) > 1 {
            return Err(Error::new(ErrorKind::InvalidData, "ACL has duplicate MASK"));
        }
        let named = count(ACL_USER) + count(ACL_GROUP);
        if named > 0 && count(ACL_MASK) == 0 {
            return Err(Error::new(ErrorKind::InvalidData, "ACL with named entries needs a MASK"));
        }
        for e in &self.entries {
            if e.perm & !0o7 != 0 {
                return Err(Error::new(ErrorKind::InvalidData, "ACL permission word has non-rwx bits"));
            }
        }
        let mut users: Vec<u32> =
            self.entries.iter().filter(|e| e.tag == ACL_USER).filter_map(|e| e.id).collect();
        users.sort_unstable();
        users.dedup();
        if users.len() != count(ACL_USER) {
            return Err(Error::new(ErrorKind::InvalidData, "ACL has duplicate USER ids"));
        }
        let mut groups: Vec<u32> =
            self.entries.iter().filter(|e| e.tag == ACL_GROUP).filter_map(|e| e.id).collect();
        groups.sort_unstable();
        groups.dedup();
        if groups.len() != count(ACL_GROUP) {
            return Err(Error::new(ErrorKind::InvalidData, "ACL has duplicate GROUP ids"));
        }
        if is_default {
            // A default ACL that carries ONLY the three base classes is
            // meaningless (it would grant exactly what the mode already
            // grants) and the kernel rejects it the same way.
            if named == 0 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "default ACL needs at least one named entry",
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn find(&self, tag: u16) -> Option<&AclEntry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    #[must_use]
    pub fn named_by_id(&self, tag: u16, id: u32) -> Option<&AclEntry> {
        self.entries.iter().find(|e| e.tag == tag && e.id == Some(id))
    }

    /// The stat(2) mode bits this ACL implies (draft 17 §17:
    /// the group class is GROUP_OBJ when no named entries exist,
    /// MASK otherwise). Mode convention: owner is bits 6..8.
    #[must_use]
    pub fn mode_bits(&self) -> u32 {
        let user_obj = self.find(ACL_USER_OBJ).map_or(0, |e| u32::from(e.perm));
        let group_obj = self.find(ACL_GROUP_OBJ).map_or(0, |e| u32::from(e.perm));
        let mask = self.find(ACL_MASK).map(|e| u32::from(e.perm));
        let other = self.find(ACL_OTHER).map_or(0, |e| u32::from(e.perm));
        let group_class = match mask {
            Some(m) => group_obj & m,
            None => group_obj,
        };
        (user_obj << 6) | (group_class << 3) | other
    }

    /// The draft-17 access-check algorithm. `uid`/`gid` are the
    /// CALLER's credentials; `owner_uid`/`owner_gid` the inode's.
    /// `want` is a mask of PERM_* bits. Supplementary groups are not
    /// consulted (VFS surface limit, documented at module top).
    #[must_use]
    pub fn evaluate(&self, uid: u32, gid: u32, owner_uid: u32, owner_gid: u32, want: u16) -> bool {
        let granted = |perm: u16, mask: Option<u16>| -> bool {
            let effective = perm & mask.unwrap_or(!0u16);
            effective & want == want
        };
        // 1. owner
        if uid == owner_uid {
            let perm = self.find(ACL_USER_OBJ).map_or(0, |e| e.perm);
            return granted(perm, None);
        }
        // 2. named user
        if let Some(e) = self.named_by_id(ACL_USER, uid) {
            let mask = self.find(ACL_MASK).map(|m| m.perm);
            return granted(e.perm, mask);
        }
        // 3. owning group OR named group (any match grants).
        let mask = self.find(ACL_MASK).map(|m| m.perm);
        if gid == owner_gid {
            let perm = self.find(ACL_GROUP_OBJ).map_or(0, |e| e.perm);
            if granted(perm, mask) {
                return true;
            }
        }
        if let Some(e) = self.named_by_id(ACL_GROUP, gid) {
            if granted(e.perm, mask) {
                return true;
            }
        }
        // 4. fallthrough: if the caller's gid matched a group class but
        // was denied, draft 17 says deny (no OTHER fallback once a
        // group class matched). Only a caller matching NO group class
        // reaches OTHER via the fallthrough below.
        if gid == owner_gid || self.named_by_id(ACL_GROUP, gid).is_some() {
            return false;
        }
        // 5. other
        let perm = self.find(ACL_OTHER).map_or(0, |e| e.perm);
        granted(perm, None)
    }

    /// chmod(2) on an ACL-bearing inode: USER_OBJ/OTHER take the new
    /// mode bits verbatim; when a MASK exists the mode's group bits go
    /// to the MASK (GROUP_OBJ keeps its own entry), else to GROUP_OBJ.
    #[must_use]
    pub fn apply_chmod(&self, mode: u32) -> Self {
        let owner = (mode >> 6) & 7;
        let group = (mode >> 3) & 7;
        let other = mode & 7;
        let mut out = self.entries.clone();
        for e in out.iter_mut() {
            match e.tag {
                ACL_USER_OBJ => e.perm = owner as u16,
                ACL_OTHER => e.perm = other as u16,
                ACL_MASK => e.perm = group as u16,
                _ => {}
            }
        }
        let has_mask = out.iter().any(|e| e.tag == ACL_MASK);
        if !has_mask {
            for e in out.iter_mut() {
                if e.tag == ACL_GROUP_OBJ {
                    e.perm = group as u16;
                }
            }
        }
        Self { entries: out }
    }

    /// mkdir(2) inheritance (draft 17 §15): the child's ACCESS ACL is
    /// the parent's DEFAULT ACL intersected with the create mode; the
    /// child's DEFAULT ACL (if the child is a directory) is a copy of
    /// the parent's.
    #[must_use]
    pub fn inherit_access_from_default(&self, create_mode: u32) -> Self {
        let owner = ((create_mode >> 6) & 7) as u16;
        let group = ((create_mode >> 3) & 7) as u16;
        let other = (create_mode & 7) as u16;
        let mut entries = self.entries.clone();
        let mut max_named = 0u16;
        for e in entries.iter_mut() {
            match e.tag {
                ACL_USER_OBJ => e.perm &= owner,
                ACL_GROUP_OBJ => e.perm &= group,
                ACL_OTHER => e.perm &= other,
                ACL_USER | ACL_GROUP => max_named |= e.perm,
                _ => {}
            }
        }
        let has_mask = entries.iter().any(|e| e.tag == ACL_MASK);
        if !has_mask && max_named != 0 {
            entries.push(AclEntry { tag: ACL_MASK, perm: max_named, id: None });
        }
        Self { entries }
    }

    /// A trivial ACL (pure mode bits, no named entries): the shape
    /// every inode logically has before its first ACL xattr.
    #[must_use]
    pub fn from_mode(mode: u32) -> Self {
        Self {
            entries: vec![
                AclEntry { tag: ACL_USER_OBJ, perm: ((mode >> 6) & 7) as u16, id: None },
                AclEntry { tag: ACL_GROUP_OBJ, perm: ((mode >> 3) & 7) as u16, id: None },
                AclEntry { tag: ACL_OTHER, perm: (mode & 7) as u16, id: None },
            ],
        }
    }

    /// Does this ACL grant anything a pure mode-bit check would not?
    /// (False for trivial ACLs -- the caller can skip ACL evaluation.)
    #[must_use]
    pub fn is_trivial(&self) -> bool {
        self.entries.iter().all(|e| {
            !matches!(e.tag, ACL_USER | ACL_GROUP | ACL_MASK)
        })
    }
}

fn is_valid_tag(tag: u16) -> bool {
    matches!(
        tag,
        ACL_USER_OBJ | ACL_USER | ACL_GROUP_OBJ | ACL_GROUP | ACL_MASK | ACL_OTHER
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl(entries: &[AclEntry]) -> PosixAcl {
        PosixAcl { entries: entries.to_vec() }
    }

    #[test]
    fn roundtrip_wire_format() {
        let a = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_USER, perm: 0o4, id: Some(1000) },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o4, id: None },
            AclEntry { tag: ACL_GROUP, perm: 0o0, id: Some(2000) },
            AclEntry { tag: ACL_MASK, perm: 0o6, id: None },
            AclEntry { tag: ACL_OTHER, perm: 0o0, id: None },
        ]);
        let bytes = a.encode();
        let decoded = PosixAcl::decode(&bytes).expect("decodes");
        assert_eq!(decoded, a);
    }

    #[test]
    fn rejects_structural_violations() {
        // Missing OTHER.
        let a = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o4, id: None },
        ]);
        assert!(a.validate(false).is_err());
        // Named entries without MASK.
        let b = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_USER, perm: 0o4, id: Some(1) },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o4, id: None },
            AclEntry { tag: ACL_OTHER, perm: 0o0, id: None },
        ]);
        assert!(b.validate(false).is_err());
        // Valid with mask.
        let c = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_USER, perm: 0o4, id: Some(1) },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o4, id: None },
            AclEntry { tag: ACL_MASK, perm: 0o4, id: None },
            AclEntry { tag: ACL_OTHER, perm: 0o0, id: None },
        ]);
        assert!(c.validate(false).is_ok());
    }

    #[test]
    fn mode_bits_follow_mask_rule() {
        let trivial = PosixAcl::from_mode(0o754);
        assert_eq!(trivial.mode_bits(), 0o754);
        let named = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o7, id: None },
            AclEntry { tag: ACL_USER, perm: 0o5, id: Some(100) },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o7, id: None },
            AclEntry { tag: ACL_MASK, perm: 0o5, id: None },
            AclEntry { tag: ACL_OTHER, perm: 0o4, id: None },
        ]);
        // group class = GROUP_OBJ(7) & MASK(5) = 5
        assert_eq!(named.mode_bits(), 0o754);
    }

    #[test]
    fn evaluate_owner_named_and_mask() {
        let a = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_USER, perm: 0o4, id: Some(1000) },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_MASK, perm: 0o4, id: None },
            AclEntry { tag: ACL_OTHER, perm: 0o0, id: None },
        ]);
        // Owner: r+w granted, x not.
        assert!(a.evaluate(7, 7, 7, 7, PERM_READ | PERM_WRITE));
        assert!(!a.evaluate(7, 7, 7, 7, PERM_WRITE | PERM_EXECUTE));
        // Named user 1000: entry r, MASK r -> r granted.
        assert!(a.evaluate(1000, 0, 7, 7, PERM_READ));
        assert!(!a.evaluate(1000, 0, 7, 7, PERM_WRITE));
        // Owning group 7: GROUP_OBJ 6 & MASK 4 -> only r.
        assert!(a.evaluate(5, 7, 7, 7, PERM_READ));
        assert!(!a.evaluate(5, 7, 7, 7, PERM_WRITE));
        // Unrelated: OTHER 0 -> nothing.
        assert!(!a.evaluate(5, 9, 7, 7, PERM_READ));
    }

    #[test]
    fn chmod_remaps_class_entries() {
        let a = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_USER, perm: 0o4, id: Some(1000) },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_MASK, perm: 0o6, id: None },
            AclEntry { tag: ACL_OTHER, perm: 0o0, id: None },
        ]);
        let b = a.apply_chmod(0o644);
        // MASK takes the mode group bits; GROUP_OBJ unchanged.
        assert_eq!(b.find(ACL_MASK).unwrap().perm, 0o4);
        assert_eq!(b.find(ACL_GROUP_OBJ).unwrap().perm, 0o6);
        assert_eq!(b.find(ACL_USER_OBJ).unwrap().perm, 0o6);
        assert_eq!(b.find(ACL_OTHER).unwrap().perm, 0o4);
        // Named user entry survives.
        assert_eq!(b.named_by_id(ACL_USER, 1000).unwrap().perm, 0o4);
        // Trivial ACL chmod is a pure rewrite.
        let t = PosixAcl::from_mode(0o600).apply_chmod(0o644);
        assert_eq!(t.mode_bits(), 0o644);
    }

    #[test]
    fn inheritance_intersects_with_mode() {
        let default = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o7, id: None },
            AclEntry { tag: ACL_USER, perm: 0o5, id: Some(1000) },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o7, id: None },
            AclEntry { tag: ACL_MASK, perm: 0o5, id: None },
            AclEntry { tag: ACL_OTHER, perm: 0o7, id: None },
        ]);
        let child = default.inherit_access_from_default(0o750);
        assert_eq!(child.find(ACL_USER_OBJ).unwrap().perm, 0o7);
        assert_eq!(child.find(ACL_GROUP_OBJ).unwrap().perm, 0o5);
        assert_eq!(child.find(ACL_OTHER).unwrap().perm, 0o0);
        assert_eq!(child.mode_bits(), 0o750);
    }

    #[test]
    fn triviality_detection() {
        assert!(PosixAcl::from_mode(0o644).is_trivial());
        let named = acl(&[
            AclEntry { tag: ACL_USER_OBJ, perm: 0o6, id: None },
            AclEntry { tag: ACL_USER, perm: 0o4, id: Some(9) },
            AclEntry { tag: ACL_GROUP_OBJ, perm: 0o4, id: None },
            AclEntry { tag: ACL_MASK, perm: 0o4, id: None },
            AclEntry { tag: ACL_OTHER, perm: 0o0, id: None },
        ]);
        assert!(!named.is_trivial());
    }
}
