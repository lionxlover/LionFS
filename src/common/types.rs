//! Newtype wrappers for the plain `u64`/`u32` identifiers used throughout
//! LionFS (inode numbers, block numbers, generation counters). The
//! existing code passes raw integers everywhere, which compiles fine but
//! makes it easy to accidentally pass a block number where an inode number
//! was expected -- both are just `u64`. These types are additive: new code
//! can opt into them for that safety; nothing existing is forced to change.

use std::fmt;

macro_rules! id_type {
    ($name:ident, $repr:ty) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub $repr);

        impl $name {
            pub const fn get(self) -> $repr {
                self.0
            }
        }

        impl From<$repr> for $name {
            fn from(v: $repr) -> Self {
                Self(v)
            }
        }

        impl From<$name> for $repr {
            fn from(v: $name) -> $repr {
                v.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(InodeId, u64);
id_type!(BlockNum, u64);
id_type!(Generation, u64);
id_type!(DeviceIndex, usize);

impl InodeId {
    pub const ROOT: InodeId = InodeId(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_to_and_from_raw() {
        let ino = InodeId::from(42u64);
        assert_eq!(ino.get(), 42);
        let raw: u64 = ino.into();
        assert_eq!(raw, 42);
    }

    #[test]
    fn distinct_id_types_do_not_implicitly_mix() {
        // Really a compile-time property (InodeId and BlockNum are
        // unrelated types, so passing one where the other is expected is a
        // type error) -- this just confirms values still round-trip.
        let ino = InodeId(7);
        let blk = BlockNum(7);
        assert_eq!(ino.get(), blk.get());
    }

    #[test]
    fn root_inode_constant_is_one() {
        assert_eq!(InodeId::ROOT.get(), 1);
    }
}
