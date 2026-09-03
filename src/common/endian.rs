//! Explicit endianness helpers.
//!
//! Known limitation worth stating plainly: `Superblock`/`Inode`/etc. derive
//! `bytemuck::Pod` and are written to disk via `bytes_of`, which reflects
//! whatever the *host* CPU's native byte order is (little-endian on every
//! platform this project currently targets/builds on). That means an image
//! written on a big-endian host would not currently be readable on a
//! little-endian one, and vice versa -- the on-disk format is not actually
//! endian-portable yet, despite that being a reasonable thing to expect
//! from a real filesystem format. Fixing that properly means changing
//! every multi-byte field access to go through explicit
//! to/from-little-endian conversions (or a wrapper type) at the point
//! structs are serialized/deserialized, which touches a lot of surface
//! area; that rewrite isn't done here. What's here are the conversion
//! primitives that rewrite would use, plus a way to confirm at runtime
//! which regime is actually in effect on a given host.
pub fn is_little_endian_host() -> bool {
    1u16.to_ne_bytes()[0] == 1
}

pub trait ToLe {
    fn to_le_bytes_vec(&self) -> Vec<u8>;
}

macro_rules! impl_to_le {
    ($t:ty) => {
        impl ToLe for $t {
            fn to_le_bytes_vec(&self) -> Vec<u8> {
                self.to_le_bytes().to_vec()
            }
        }
    };
}
impl_to_le!(u16);
impl_to_le!(u32);
impl_to_le!(u64);
impl_to_le!(i16);
impl_to_le!(i32);
impl_to_le!(i64);

/// Byte-swaps every value in `data` in place, treating it as an array of
/// `N`-byte little-endian words. Useful for a hypothetical future
/// migration path (converting an image between endiannesses), not
/// currently invoked anywhere in the mount/read path.
pub fn swap_words_in_place<const N: usize>(data: &mut [u8]) {
    for chunk in data.chunks_exact_mut(N) {
        chunk.reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_bytes_round_trip() {
        let v: u32 = 0x0102_0304;
        let bytes = v.to_le_bytes_vec();
        assert_eq!(bytes, vec![0x04, 0x03, 0x02, 0x01]);
        assert_eq!(u32::from_le_bytes(bytes.try_into().unwrap()), v);
    }

    #[test]
    fn swap_words_reverses_each_word_independently() {
        let mut data = vec![1, 2, 3, 4, 5, 6, 7, 8];
        swap_words_in_place::<4>(&mut data);
        assert_eq!(data, vec![4, 3, 2, 1, 8, 7, 6, 5]);
    }
}
