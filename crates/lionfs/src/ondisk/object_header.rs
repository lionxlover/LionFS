//! A small self-describing header format for future extensible on-disk
//! objects (e.g. extended attributes, or arbitrary metadata blobs
//! referenced by `object::tree::ObjectTree`, which indexes objects by id
//! but doesn't currently define what an object's own on-disk bytes look
//! like). Not wired into anything yet -- `ObjectTree` today stores fixed
//! `ObjectEntry` records directly rather than pointers to headered blobs
//! -- but a real, self-contained format ready for that.

use bytemuck::{Pod, Zeroable};

pub const OBJECT_MAGIC: u32 = 0x4C4F_424A; // "LOBJ"

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Unknown = 0,
    ExtendedAttribute = 1,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ObjectHeader {
    pub magic: u32,
    pub kind: u8,
    pub version: u8,
    pub _padding: u16,
    pub payload_len: u32,
    pub checksum: u32,
}

impl ObjectHeader {
    pub fn new(kind: ObjectKind, payload: &[u8]) -> Self {
        let checksum = crate::common::checksum::fletcher32(payload);
        Self {
            magic: OBJECT_MAGIC,
            kind: kind as u8,
            version: 1,
            _padding: 0,
            payload_len: payload.len() as u32,
            checksum,
        }
    }

    pub fn verify(&self, payload: &[u8]) -> bool {
        self.magic == OBJECT_MAGIC
            && self.payload_len as usize == payload.len()
            && self.checksum == crate::common::checksum::fletcher32(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_verifies_its_own_payload() {
        let payload = b"some extended attribute value";
        let header = ObjectHeader::new(ObjectKind::ExtendedAttribute, payload);
        assert!(header.verify(payload));
    }

    #[test]
    fn header_rejects_a_different_payload() {
        let header = ObjectHeader::new(ObjectKind::ExtendedAttribute, b"original");
        assert!(!header.verify(b"tampered!"));
    }

    #[test]
    fn header_rejects_wrong_length_payload() {
        let header = ObjectHeader::new(ObjectKind::ExtendedAttribute, b"12345");
        assert!(!header.verify(b"1234"));
    }
}
