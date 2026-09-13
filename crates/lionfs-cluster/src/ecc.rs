//! Systematic Reed-Solomon erasure coding over GF(2^8) (spec §5.3).
//!
//! A stripe is `[D1 | D2 | ... | Dk | P1 | ... | Pm]`; any `m` shards may be
//! lost and the stripe is still fully recoverable from any `k` survivors:
//!
//! ```text
//! D_i = decode(any k of (D1..Dk, P1..Pm))
//! ```
//!
//! Construction (Backblaze-style): build a Vandermonde matrix `V` (n×k),
//! left-multiply by `V_top^{-1}` so the top k×k block becomes the identity.
//! The resulting matrix `M` is the *systematic* encode matrix: the first k
//! rows reproduce the data shards, the last m rows produce the parities.
//! Recovery inverts whatever k surviving rows remain.
//!
//! Default profile: `k=8, m=3` (37.5% overhead), tunable per-directory via
//! the `helix.redundancy=k:m` xattr.

#[derive(Debug, thiserror::Error)]
pub enum EccError {
    #[error("invalid RS(n={0}, k={1}): need 1 <= k < n <= 255")]
    InvalidProfile(usize, usize),
    #[error("shard length mismatch: expected {expected}, got {got}")]
    ShardLength { expected: usize, got: usize },
    #[error("not enough surviving shards: need {need}, have {have}")]
    TooFewSurvivors { need: usize, have: usize },
    #[error("matrix is singular — cannot invert")]
    SingularMatrix,
}

/// GF(2^8) arithmetic with the AES polynomial 0x11D (x^8+x^4+x^3+x^2+1).
mod gf {
    /// exp/log tables: exp[i] = 2^i, log[2^i] = i.
    pub struct Gf256 {
        pub exp: [u8; 512],
        pub log: [u8; 256],
    }

    impl Gf256 {
        pub const fn new() -> Self {
            // const-compatible table construction.
            let mut exp = [0u8; 512];
            let mut log = [0u8; 256];
            let mut x: u16 = 1;
            let mut i = 0;
            while i < 255 {
                exp[i] = x as u8;
                log[x as usize] = i as u8;
                x <<= 1;
                if x & 0x100 != 0 {
                    x ^= 0x11D; // reduce by the AES polynomial
                }
                i += 1;
            }
            // exp wraps: exp[i+255] == exp[i]
            let mut i = 255;
            while i < 512 {
                exp[i] = exp[i - 255];
                i += 1;
            }
            Self { exp, log }
        }

        #[inline]
        pub fn mul(&self, a: u8, b: u8) -> u8 {
            if a == 0 || b == 0 {
                0
            } else {
                self.exp[(self.log[a as usize] as usize) + (self.log[b as usize] as usize)]
            }
        }

        #[cfg(test)]
        #[inline]
        pub fn div(&self, a: u8, b: u8) -> u8 {
            debug_assert!(b != 0);
            if a == 0 {
                0
            } else {
                let mut l = (self.log[a as usize] as i32) - (self.log[b as usize] as i32);
                if l < 0 {
                    l += 255;
                }
                self.exp[l as usize]
            }
        }

        #[inline]
        pub fn inv(&self, a: u8) -> u8 {
            debug_assert!(a != 0);
            self.exp[255 - (self.log[a as usize] as usize)]
        }
    }

    /// Shared global GF tables (cheap to construct, but this avoids rebuilds).
    pub fn gf() -> &'static Gf256 {
        use std::sync::OnceLock;
        static GF: OnceLock<Gf256> = OnceLock::new();
        GF.get_or_init(Gf256::new)
    }
}

/// A systematic Reed-Solomon codec for one (k, m) profile.
#[derive(Clone)]
pub struct RsCodec {
    k: usize,
    m: usize,
    /// Encode matrix: n rows × k columns. Rows 0..k are the identity;
    /// rows k..n are the parity rows.
    encode_matrix: Vec<Vec<u8>>,
    /// Per-parity-row multiplication tables: `mul_tables[j][c][x] =
    /// encode_matrix[k+j][c] · x`. Replaces the log/exp GF multiply in the
    /// innermost encode loop with a single table lookup (the ISA-L
    /// technique, minus the SIMD). m·k·256 bytes ≈ 6 KiB for (8, 3).
    mul_tables: Vec<Vec<[u8; 256]>>,
}

impl RsCodec {
    /// Create a codec for RS(n = k+m, k).
    pub fn new(k: usize, m: usize) -> Result<Self, EccError> {
        if k == 0 || m == 0 || k + m > 255 {
            return Err(EccError::InvalidProfile(k + m, k));
        }
        let n = k + m;

        // Vandermonde matrix V: V[r][c] = alpha_r^c  (alpha_r = r-th field elem)
        let g = gf::gf();
        let mut vandermonde = vec![vec![1u8; k]; n];
        for (r, row) in vandermonde.iter_mut().enumerate() {
            for c in 1..k {
                row[c] = g.mul(row[c - 1], r as u8);
            }
        }

        // Invert the top k×k block, then M = V · V_top^{-1}  → top is identity.
        // (Any k rows of M are then invertible: they are (k distinct Vandermonde
        // rows) · V_top^{-1}, a product of two invertible matrices.)
        let top_inv = invert_matrix(&vandermonde[..k])?;
        let mut encode_matrix = vec![vec![0u8; k]; n];
        for r in 0..n {
            for c in 0..k {
                let mut acc = 0u8;
                for t in 0..k {
                    acc ^= g.mul(vandermonde[r][t], top_inv[t][c]);
                }
                encode_matrix[r][c] = acc;
            }
        }

        // Precompute the parity multiply tables once per codec.
        let mul_tables = (0..m)
            .map(|j| {
                (0..k)
                    .map(|c| mul_table(g, encode_matrix[k + j][c]))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        Ok(Self { k, m, encode_matrix, mul_tables })
    }

    /// (k, m) profile of this codec.
    pub fn profile(&self) -> (usize, usize) {
        (self.k, self.m)
    }

    /// Compute `m` parity shards for `k` equal-length data shards.
    pub fn encode(&self, data_shards: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, EccError> {
        if data_shards.len() != self.k {
            return Err(EccError::InvalidProfile(data_shards.len() + self.m, self.k));
        }
        let len = data_shards.first().map(|s| s.len()).unwrap_or(0);
        for s in data_shards.iter() {
            if s.len() != len {
                return Err(EccError::ShardLength { expected: len, got: s.len() });
            }
        }
        // Column-at-a-time: for every data column c, XOR table[c][byte]
        // into every parity row. The inner loop is two contiguous slices and
        // a table lookup — no per-byte GF arithmetic, bounds checks elided,
        // and the access pattern is cache-friendly enough for LLVM to
        // auto-vectorize the XOR.
        let mut parity = vec![vec![0u8; len]; self.m];
        for (c, ds) in data_shards.iter().enumerate() {
            for (j, prow) in parity.iter_mut().enumerate() {
                let t = &self.mul_tables[j][c];
                for (p, &b) in prow.iter_mut().zip(ds.iter()) {
                    *p ^= t[b as usize];
                }
            }
        }
        Ok(parity)
    }

    /// Recover the `k` data shards from any `k` surviving shards.
    ///
    /// `shards` must have exactly `n` entries (data shards first, parity
    /// after); `None` marks a lost shard.
    pub fn decode(&self, shards: &[Option<Vec<u8>>]) -> Result<Vec<Vec<u8>>, EccError> {
        let n = self.k + self.m;
        if shards.len() != n {
            return Err(EccError::InvalidProfile(shards.len(), self.k));
        }
        let mut survivors: Vec<(usize, &Vec<u8>)> = Vec::with_capacity(n);
        for (idx, s) in shards.iter().enumerate() {
            if let Some(data) = s {
                survivors.push((idx, data));
            }
        }
        if survivors.len() < self.k {
            return Err(EccError::TooFewSurvivors {
                need: self.k,
                have: survivors.len(),
            });
        }
        let len = survivors[0].1.len();
        for (_, s) in &survivors {
            if s.len() != len {
                return Err(EccError::ShardLength { expected: len, got: s.len() });
            }
        }
        survivors.truncate(self.k);

        // Build the k×k matrix of surviving encode rows and invert it.
        let mut sub: Vec<Vec<u8>> = Vec::with_capacity(self.k);
        for (row_idx, _shard) in &survivors {
            // A surviving *data* shard contributes the identity row r=r;
            // a surviving *parity* shard contributes encode row k+j.
            let src = self.encode_matrix[*row_idx].clone();
            sub.push(src);
        }
        let sub_inv = invert_matrix(&sub)?;

        // original_data = sub_inv · surviving_shards  (table-driven).
        let g = gf::gf();
        let mut data = vec![vec![0u8; len]; self.k];
        for (r, drow) in data.iter_mut().enumerate() {
            let row_tables: Vec<[u8; 256]> = (0..self.k)
                .map(|c| mul_table(g, sub_inv[r][c]))
                .collect();
            for (c, (_, shard)) in survivors.iter().enumerate() {
                let t = &row_tables[c];
                for (d, &b) in drow.iter_mut().zip(shard.iter()) {
                    *d ^= t[b as usize];
                }
            }
        }
        Ok(data)
    }

    /// Reconstruct one lost shard from survivors without decoding everything.
    ///
    /// Returns the reconstructed bytes of `lost_index` (data or parity index).
    pub fn reconstruct(&self, shards: &[Option<Vec<u8>>], lost_index: usize) -> Result<Vec<u8>, EccError> {
        let n = self.k + self.m;
        if shards.len() != n || lost_index >= n || shards[lost_index].is_some() {
            return Err(EccError::InvalidProfile(shards.len(), lost_index));
        }
        let data = self.decode(shards)?;
        if lost_index < self.k {
            Ok(data[lost_index].clone())
        } else {
            // Recompute the parity shard.
            let parity = self.encode(&data)?;
            Ok(parity[lost_index - self.k].clone())
        }
    }
}

/// `row ^= factor · pivot_row` over GF(2^8), as a slice zip.
#[inline]
fn eliminate(row: &mut [u8], pivot_row: &[u8], factor: u8, g: &gf::Gf256) {
    for (dst, &src) in row.iter_mut().zip(pivot_row.iter()) {
        *dst ^= g.mul(factor, src);
    }
}

/// Full 256-entry multiply table for one GF(2^8) coefficient.
fn mul_table(g: &gf::Gf256, coeff: u8) -> [u8; 256] {
    let mut t = [0u8; 256];
    for (x, slot) in t.iter_mut().enumerate() {
        *slot = g.mul(coeff, x as u8);
    }
    t
}

/// Invert a k×k matrix over GF(2^8) by Gauss-Jordan elimination.
fn invert_matrix(matrix: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, EccError> {
    let k = matrix.len();
    if k == 0 || matrix.iter().any(|r| r.len() != k) {
        return Err(EccError::SingularMatrix);
    }
    let g = gf::gf();

    // [A | I]
    let mut work: Vec<Vec<u8>> = matrix
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let mut augmented = row.clone();
            augmented.extend((0..k).map(|c| if c == r { 1u8 } else { 0u8 }));
            augmented
        })
        .collect();

    for col in 0..k {
        // Find a pivot row with a nonzero entry in this column.
        let pivot = work
            .iter()
            .skip(col)
            .position(|row| row[col] != 0)
            .map(|offset| col + offset)
            .ok_or(EccError::SingularMatrix)?;
        work.swap(col, pivot);

        // Scale pivot row so the diagonal becomes 1.
        let inv = g.inv(work[col][col]);
        for v in work[col].iter_mut() {
            *v = g.mul(*v, inv);
        }

        // Eliminate the column from every other row.
        for r in 0..k {
            if r == col || work[r][col] == 0 {
                continue;
            }
            let factor = work[r][col];
            if r < col {
                let (lo, hi) = work.split_at_mut(col);
                eliminate(&mut lo[r], &hi[0], factor, g);
            } else {
                let (lo, hi) = work.split_at_mut(r);
                eliminate(&mut hi[0], &lo[col], factor, g);
            }
        }
    }

    Ok((0..k).map(|r| work[r][k..2 * k].to_vec()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
        // xorshift for deterministic pseudo-random data.
        let mut s = seed;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    }

    #[test]
    fn gf256_arithmetic() {
        let g = gf::gf();
        assert_eq!(g.mul(1, 5), 5);
        assert_eq!(g.mul(0, 5), 0);
        for a in 1u8..=255 {
            assert_eq!(g.mul(a, g.inv(a)), 1, "a={a}");
        }
        for a in 1u8..=255 {
            for b in 1u8..=16 {
                assert_eq!(g.div(g.mul(a, b), b), a);
            }
        }
    }

    #[test]
    fn roundtrip_various_profiles() {
        for (k, m) in [(2, 1), (4, 2), (8, 3), (10, 4)] {
            let codec = RsCodec::new(k, m).unwrap();
            let shard_len = 1024;
            let data: Vec<Vec<u8>> = (0..k).map(|i| random_bytes(shard_len, 100 + i as u64)).collect();
            let parity = codec.encode(&data).unwrap();

            // Lose every combination of up to m shards.
            let n = k + m;
            let mut lost_masks: Vec<u32> = Vec::new();
            for mask in 0u32..(1u32 << n) {
                if mask.count_ones() as usize <= m {
                    lost_masks.push(mask);
                }
            }
            let masks: Vec<u32> = if lost_masks.len() > 64 {
                // sample to keep the test fast for (10,4): every 137th mask
                lost_masks.into_iter().step_by(137).collect()
            } else {
                lost_masks
            };
            for mask in masks {
                let mut shards: Vec<Option<Vec<u8>>> = (0..k)
                    .map(|i| Some(data[i].clone()))
                    .chain((0..m).map(|j| Some(parity[j].clone())))
                    .collect();
                for (idx, slot) in shards.iter_mut().enumerate() {
                    if mask & (1 << idx) != 0 {
                        *slot = None;
                    }
                }
                let recovered = codec.decode(&shards).unwrap();
                assert_eq!(recovered, data, "k={k} m={m} mask={mask:#b}");
            }
        }
    }

    #[test]
    fn too_many_losses_are_detected() {
        let codec = RsCodec::new(4, 2).unwrap();
        let data: Vec<Vec<u8>> = (0..4).map(|i| random_bytes(256, 7 + i as u64)).collect();
        let parity = codec.encode(&data).unwrap();
        let shards: Vec<Option<Vec<u8>>> = vec![
            Some(data[0].clone()),
            None,
            None,
            Some(data[3].clone()),
            None,
            Some(parity[1].clone()),
        ];
        assert!(codec.decode(&shards).is_err());
    }

    #[test]
    fn reconstruct_single_lost_shard() {
        let codec = RsCodec::new(4, 2).unwrap();
        let data: Vec<Vec<u8>> = (0..4).map(|i| random_bytes(512, 42 + i as u64)).collect();
        let parity = codec.encode(&data).unwrap();

        // Lose parity shard 0.
        let mut shards: Vec<Option<Vec<u8>>> =
            (0..4).map(|i| Some(data[i].clone())).chain((0..2).map(|j| Some(parity[j].clone()))).collect();
        shards[4] = None;
        assert_eq!(codec.reconstruct(&shards, 4).unwrap(), parity[0]);

        // Lose data shard 2.
        shards[4] = Some(parity[0].clone());
        shards[2] = None;
        assert_eq!(codec.reconstruct(&shards, 2).unwrap(), data[2]);
    }

    #[test]
    fn systematic_identity() {
        // With zero losses, decode must return the data unchanged.
        let codec = RsCodec::new(8, 3).unwrap();
        let data: Vec<Vec<u8>> = (0..8).map(|i| random_bytes(64, i as u64 + 900)).collect();
        let parity = codec.encode(&data).unwrap();
        let shards: Vec<Option<Vec<u8>>> =
            (0..8).map(|i| Some(data[i].clone())).chain((0..3).map(|j| Some(parity[j].clone()))).collect();
        assert_eq!(codec.decode(&shards).unwrap(), data);
    }

    #[test]
    fn invalid_profiles_rejected() {
        assert!(RsCodec::new(0, 3).is_err());
        assert!(RsCodec::new(8, 0).is_err());
        assert!(RsCodec::new(200, 100).is_err());
    }
}
