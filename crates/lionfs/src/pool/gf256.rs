//! GF(2^8) (Galois field of 256 elements) arithmetic, used by RAID6's Q
//! (second) parity. This is the same field and generator used by the
//! standard reference construction for RAID6 (H. Peter Anvin, "The
//! Mathematics of RAID-6") and by AES: primitive polynomial
//! x^8 + x^4 + x^3 + x^2 + 1 (0x11D), generator g = 2.
//!
//! Addition and subtraction in this field are both just XOR. Multiplication
//! and division go through precomputed log/exp tables (the standard,
//! textbook way to make GF(256) arithmetic cheap): `log[a]` is the exponent
//! `e` such that `g^e == a`, and `exp[e] == g^e`, so `a * b == exp[log[a] +
//! log[b] mod 255]`.

const POLY: u16 = 0x11D;

fn build_tables() -> ([u8; 256], [u8; 256]) {
    let mut exp = [0u8; 256];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    for i in 0..255usize {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
    }
    exp[255] = exp[0]; // convenience wraparound, log[0] is left as 0 (unused: 0 has no log)
    (exp, log)
}

struct Tables {
    exp: [u8; 256],
    log: [u8; 256],
}

fn tables() -> &'static Tables {
    use std::sync::OnceLock;
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let (exp, log) = build_tables();
        Tables { exp, log }
    })
}

/// GF(256) multiplication.
pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = tables();
    let sum = t.log[a as usize] as u16 + t.log[b as usize] as u16;
    t.exp[(sum % 255) as usize]
}

/// `g^power` in GF(256), i.e. the coefficient used for the `power`-th data
/// block when computing Q parity.
pub fn pow(power: u32) -> u8 {
    tables().exp[(power % 255) as usize]
}

/// Multiplicative inverse in GF(256); `a` must be nonzero.
pub fn inv(a: u8) -> u8 {
    assert!(a != 0, "GF(256) zero has no multiplicative inverse");
    let t = tables();
    t.exp[((255 - t.log[a as usize] as u32) % 255) as usize]
}

/// GF(256) division: `a / b`, `b` must be nonzero.
pub fn div(a: u8, b: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    mul(a, inv(b))
}

/// XORs `src` into `dst` byte-wise (GF(256) addition), in place.
pub fn xor_into(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d ^= s;
    }
}

/// Precomputed multiplication table for one coefficient: `table[b] =
/// coeff * b` in GF(256). Building it costs 256 multiplications once;
/// every subsequent byte is a single lookup -- which matters on the
/// parity hot path, where the per-byte `mul` (two table lookups plus a
/// OnceLock) dominated RAID6 parity cost.
pub fn mul_table(coeff: u8) -> [u8; 256] {
    mul_tables()[coeff as usize]
}

/// The full 256x256 multiplication table set (64 KiB), built once per
/// process and shared by every coefficient lookup afterwards.
///
/// Phase 3.2 fix: `mul_xor_into` (and every `mul_table` caller) used to
/// rebuild the 256-entry table for its coefficient ON EVERY CALL -- 256
/// field multiplications per 4 KiB block, pure overhead repeated for
/// every stripe, every incremental parity update, every rebuild. The
/// tables are tiny (64 KiB total) and immutable, so they are built once
/// here and served from the OnceLock for the process lifetime. The
/// per-call build was measurable on the RAID6 commit path, where
/// Q-syndrome math dominates (see docs/benchmarks.md).
fn mul_tables() -> &'static [[u8; 256]; 256] {
    use std::sync::OnceLock;
    static TABLES: OnceLock<Box<[[u8; 256]; 256]>> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut set = vec![[0u8; 256]; 256];
        for coeff in 1..=255u8 {
            set[coeff as usize] = mul_table_uncached(coeff);
        }
        // set[0] stays all-zero: 0 * b = 0.
        let boxed: Box<[[u8; 256]; 256]> = set.into_boxed_slice().try_into().ok().unwrap();
        boxed
    })
}

/// The original per-coefficient builder, kept for the one-shot lazy set
/// construction above (and for tests that want to verify the cached
/// tables against a fresh build).
fn mul_table_uncached(coeff: u8) -> [u8; 256] {
    let mut table = [0u8; 256];
    if coeff == 0 {
        return table; // 0 * anything = 0
    }
    let t = tables();
    let lc = t.log[coeff as usize] as u16;
    for b in 1..=255u8 {
        let sum = lc + t.log[b as usize] as u16;
        table[b as usize] = t.exp[(sum % 255) as usize];
    }
    table
}

/// Computes `dst[i] ^= coeff * src[i]` for every byte (a full-block
/// multiply-accumulate over GF(256)), in place.
///
/// Phase 3.2: two changes. (1) The coefficient table comes from the
/// process-wide cached set -- no more 256-multiplication rebuild per
/// call. (2) The byte loop is unrolled 8-wide over `chunks_exact` with
/// fixed indices, which lets LLVM drop the per-byte bounds checks and
/// keeps the loop in registers; the 256-entry lookup itself is a gather
/// that does not auto-vectorize without AVX2 intrinsics, so this stays
/// scalar by design (the honest limit of a no-intrinsics, three-OS
/// portable build).
pub fn mul_xor_into(dst: &mut [u8], src: &[u8], coeff: u8) {
    let table = &mul_tables()[coeff as usize];
    let n = dst.len().min(src.len());
    let (d, s) = (&mut dst[..n], &src[..n]);

    let mut d_chunks = d.chunks_exact_mut(8);
    let mut s_chunks = s.chunks_exact(8);
    while let (Some(d8), Some(s8)) = (d_chunks.next(), s_chunks.next()) {
        d8[0] ^= table[s8[0] as usize];
        d8[1] ^= table[s8[1] as usize];
        d8[2] ^= table[s8[2] as usize];
        d8[3] ^= table[s8[3] as usize];
        d8[4] ^= table[s8[4] as usize];
        d8[5] ^= table[s8[5] as usize];
        d8[6] ^= table[s8[6] as usize];
        d8[7] ^= table[s8[7] as usize];
    }
    let d_rest = d_chunks.into_remainder();
    let s_rest = s_chunks.remainder();
    for (dv, sv) in d_rest.iter_mut().zip(s_rest.iter()) {
        *dv ^= table[*sv as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_by_one_is_identity() {
        for a in 0..=255u8 {
            assert_eq!(mul(a, 1), a);
        }
    }

    #[test]
    fn mul_by_zero_is_zero() {
        for a in 0..=255u8 {
            assert_eq!(mul(a, 0), 0);
            assert_eq!(mul(0, a), 0);
        }
    }

    #[test]
    fn inverse_round_trips() {
        for a in 1..=255u8 {
            let inv_a = inv(a);
            assert_eq!(mul(a, inv_a), 1, "a={a} inv={inv_a}");
        }
    }

    #[test]
    fn division_is_inverse_of_multiplication() {
        for a in 1..=255u8 {
            for b in 1..=255u8 {
                let product = mul(a, b);
                assert_eq!(div(product, b), a);
            }
        }
    }

    #[test]
    fn pow_matches_repeated_multiplication() {
        let mut acc = 1u8;
        for p in 0..16u32 {
            assert_eq!(pow(p), acc);
            acc = mul(acc, 2);
        }
    }

    #[test]
    fn multiplication_is_commutative_and_distributes_over_xor() {
        let (a, b, c) = (0x53u8, 0xCAu8, 0x11u8);
        assert_eq!(mul(a, b), mul(b, a));
        // a*(b^c) == (a*b)^(a*c)
        assert_eq!(mul(a, b ^ c), mul(a, b) ^ mul(a, c));
    }

    #[test]
    fn cached_tables_match_fresh_builds_for_every_coefficient() {
        for coeff in 0..=255u8 {
            assert_eq!(mul_table(coeff), mul_table_uncached(coeff));
        }
    }

    #[test]
    fn mul_xor_into_matches_scalar_reference_on_odd_lengths() {
        let mut scalar = vec![0x5Au8; 4097];
        let mut dst = scalar.clone();
        let src: Vec<u8> = (0..4097u32).map(|i| (i * 7 + 13) as u8).collect();
        let coeffs = [1u8, 2, 0x53, 0xFF, 0];
        for &c in &coeffs {
            for (d, s) in scalar.iter_mut().zip(src.iter()) {
                *d ^= mul(*s, c);
            }
            mul_xor_into(&mut dst, &src, c);
            assert_eq!(dst, scalar, "coeff {c}");
        }
    }

    #[test]
    fn mul_xor_into_handles_mismatched_lengths_by_the_shorter() {
        let mut dst = vec![0u8; 16];
        let src = vec![1u8, 2, 3];
        mul_xor_into(&mut dst, &src, 2);
        // Only the first 3 bytes are touched (2*1=2, 2*2=4, 2*3=6).
        assert_eq!(&dst[..3], &[2, 4, 6]);
        assert_eq!(&dst[3..], &[0u8; 13]);
    }
}
