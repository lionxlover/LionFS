//! A real random (v4) UUID generator, used for `Superblock::pool_uuid` and
//! anywhere else LionFS needs an identifier that's overwhelmingly unlikely
//! to collide with another filesystem's. Uses the OS CSPRNG
//! (`security::encryption::fill_random`) rather than a `uuid` crate
//! dependency LionFS doesn't otherwise need.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Uuid(pub [u8; 16]);

impl Uuid {
    /// A random, RFC 4122 version-4 UUID.
    pub fn new_v4() -> std::io::Result<Self> {
        let mut bytes = [0u8; 16];
        crate::security::encryption::fill_random(&mut bytes)?;
        // Set version (4) and variant (RFC 4122) bits per the spec.
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        Ok(Self(bytes))
    }

    pub const fn nil() -> Self {
        Self([0; 16])
    }

    pub fn is_nil(&self) -> bool {
        self.0 == [0; 16]
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_uuids_are_not_nil_and_differ() {
        let a = Uuid::new_v4().unwrap();
        let b = Uuid::new_v4().unwrap();
        assert!(!a.is_nil());
        assert!(!b.is_nil());
        assert_ne!(a, b);
    }

    #[test]
    fn version_and_variant_bits_are_set() {
        let u = Uuid::new_v4().unwrap();
        assert_eq!(u.0[6] & 0xF0, 0x40);
        assert_eq!(u.0[8] & 0xC0, 0x80);
    }

    #[test]
    fn display_format_has_standard_dashes() {
        let u = Uuid::from_bytes([
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88,
        ]);
        let s = u.to_string();
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(&s[0..8], "12345678");
    }
}
