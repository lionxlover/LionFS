//! Generalized Reed-Solomon erasure coding (RFC-002 §5.3: "autonomous
//! Reed-Solomon erasure coding or parity repair").
//!
//! The 1.x RAID engine covers RAID5 (single parity P) and RAID6
//! (dual parity P+Q) -- RS cases k=n-1 and k=n-2. This module
//! generalizes to **any (n, k)**: an RS(n, k) code stores k data shards
//! plus (n-k) parity shards and reconstructs from any k surviving
//! shards. That is the machinery the autonomous healer needs for wide
//! pools: an RS(10, 6) pool survives 4 concurrent device losses at 60%
//! storage efficiency, which no mirror/RAID6 topology matches.
//!
//! Encoding matrix: a **Vandermonde matrix** over GF(2^8), row-reduced
//! to systematic form (first k rows = identity) so data shards are the
//! data itself and parity is a linear combination. Reconstruction
//! solves the k x k linear system over the surviving shards with
//! Gaussian elimination in the field (the 1.x `pool::gf256` tables).
//!
//! Correctness strategy: property tests with random erasure sets --
//! the same discipline the 1.x RAID6 equivalence tests (200 rounds)
//! used, because erasure-code bugs are exactly the kind that hide
//! behind the cases you thought to check.

use crate::pool::gf256;

/// An RS(n, k) code: n total shards, k data shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsCode {
    pub n: usize,
    pub k: usize,
}

impl RsCode {
    /// Creates an RS(n, k) code. Panics on degenerate parameters (n >
    /// k >= 1, n <= 255 -- the GF(256) byte limit; for wider pools the
    /// bytes-per-word grows, out of scope here and stated so).
    ///
    /// # Panics
    /// If `k == 0`, `n <= k`, or `n > 255`.
    #[must_use]
    pub fn new(n: usize, k: usize) -> Self {
        assert!(k > 0, "RS needs at least one data shard");
        assert!(n > k, "RS needs at least one parity shard (n > k)");
        assert!(n <= 255, "GF(256) codes address at most 255 shards");
        Self { n, k }
    }

    /// Parity shard count.
    #[must_use]
    pub fn parity(&self) -> usize {
        self.n - self.k
    }

    /// How many concurrent erasures this code tolerates.
    #[must_use]
    pub fn tolerates(&self) -> usize {
        self.parity()
    }

    /// Builds the systematic encoding matrix: an n x k matrix whose
    /// first k rows are the identity (data shards pass through) and
    /// whose remaining rows mix data into parity.
    ///
    /// Construction (the standard Vandermonde-derived systematic form,
    /// as used by Jerasure/Backblaze-class libraries): build the full
    /// n x k Vandermonde `V`, invert its top k x k block `V_k`, and
    /// right-multiply: `M = V * V_k^-1`. The top of `M` is then the
    /// identity, and -- because right-multiplication by an invertible
    /// matrix preserves row independence -- the MDS property carries
    /// over: any k rows of `M` are linearly independent, which is what
    /// reconstruction from any k surviving shards requires.
    #[must_use]
    pub fn encoding_matrix(&self) -> Vec<Vec<u8>> {
        // Vandermonde: row r = [1, x_r, x_r^2, ..., x_r^(k-1)] with
        // x_r = r+1 -- distinct nonzero evaluation points per ROW. It is
        // the row-distinctness that gives the MDS property (any k rows
        // of a true Vandermonde are independent: the k x k minor is a
        // Vandermonde determinant, nonzero for distinct points).
        let v: Vec<Vec<u8>> = (0..self.n)
            .map(|r| {
                let x = (r + 1) as u8;
                (0..self.k)
                    .map(|c| {
                        let mut acc = 1u8;
                        for _ in 0..c {
                            acc = gf256::mul(acc, x);
                        }
                        acc
                    })
                    .collect()
            })
            .collect();
        let top: Vec<Vec<u8>> = v[..self.k].to_vec();
        let top_inv = invert_gf256(&top, self.k);
        mat_mul(&v, &top_inv, self.n, self.k, self.k)
    }

    /// Encodes `data_shards` (each the same length) into `n` shards:
    /// the first k are the data verbatim, the remaining are parity.
    ///
    /// # Panics
    /// If `data_shards.len() != k` or shard lengths are inconsistent.
    #[must_use]
    pub fn encode(&self, data_shards: &[Vec<u8>]) -> Vec<Vec<u8>> {
        assert_eq!(data_shards.len(), self.k, "need exactly k data shards");
        let len = data_shards[0].len();
        assert!(
            data_shards.iter().all(|s| s.len() == len),
            "shards must be equal length"
        );
        let matrix = self.encoding_matrix();
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(self.n);
        for (r, row) in matrix.iter().enumerate() {
            if r < self.k {
                // Systematic: data shard passes through.
                out.push(data_shards[r].clone());
            } else {
                let mut parity = vec![0u8; len];
                for (c, &coeff) in row.iter().enumerate() {
                    if coeff == 0 {
                        continue;
                    }
                    let table = gf256::mul_table(coeff);
                    for (p, &b) in parity.iter_mut().zip(data_shards[c].iter()) {
                        *p ^= table[b as usize];
                    }
                }
                out.push(parity);
            }
        }
        out
    }

    /// Reconstructs the full shard set from `surviving` -- a map of
    /// shard index -> shard bytes for any k of the n indices. Returns
    /// all n shards (data first).
    ///
    /// # Panics
    /// If fewer than k shards survive or lengths mismatch.
    #[must_use]
    pub fn reconstruct(&self, surviving: &[(usize, Vec<u8>)]) -> Vec<Vec<u8>> {
        assert!(
            surviving.len() >= self.k,
            "need at least k shards to reconstruct"
        );
        let len = surviving[0].1.len();
        assert!(
            surviving.iter().all(|(_, s)| s.len() == len),
            "shards must be equal length"
        );
        let matrix = self.encoding_matrix();

        // Build the k x k system: rows = surviving shards' matrix rows,
        // restricted to the first k columns (data coefficients).
        let mut a: Vec<Vec<u8>> = Vec::with_capacity(self.k);
        let mut b: Vec<Vec<u8>> = Vec::with_capacity(self.k);
        for &(idx, ref shard) in surviving.iter().take(self.k) {
            a.push(matrix[idx][..self.k].to_vec());
            b.push(shard.clone());
        }

        // Solve a * x = b over GF(256), where x = the k data shards.
        let data = solve_gf256(&mut a, &mut b, self.k, len);

        // Re-encode the rest.
        let mut full = Vec::with_capacity(self.n);
        for i in 0..self.k {
            full.push(data[i].clone());
        }
        // Parity rows of the systematic matrix.
        let parity: Vec<Vec<u8>> = (self.k..self.n).map(|r| matrix[r].clone()).collect();
        for row in parity {
            let mut p = vec![0u8; len];
            for (c, &coeff) in row.iter().enumerate().take(self.k) {
                if coeff == 0 {
                    continue;
                }
                let table = gf256::mul_table(coeff);
                for (pv, &dv) in p.iter_mut().zip(data[c].iter()) {
                    *pv ^= table[dv as usize];
                }
            }
            full.push(p);
        }
        full
    }
}

/// Inverts a k x k matrix over GF(256) by Gauss-Jordan elimination with
/// an augmented identity. Panics if the matrix is singular (the
/// Vandermonde top block is invertible by construction).
fn invert_gf256(m: &[Vec<u8>], k: usize) -> Vec<Vec<u8>> {
    let mut a = m.to_vec();
    let mut inv: Vec<Vec<u8>> = (0..k)
        .map(|i| (0..k).map(|j| u8::from(i == j)).collect())
        .collect();
    for col in 0..k {
        let pivot = (col..k)
            .find(|&r| a[r][col] != 0)
            .expect("matrix is invertible");
        a.swap(col, pivot);
        inv.swap(col, pivot);
        let scale = gf256::inv(a[col][col]);
        if scale != 1 {
            let t = gf256::mul_table(scale);
            for v in a[col].iter_mut() {
                *v = t[*v as usize];
            }
            for v in inv[col].iter_mut() {
                *v = t[*v as usize];
            }
        }
        for r in 0..k {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                let t = gf256::mul_table(f);
                for c in 0..k {
                    a[r][c] ^= t[a[col][c] as usize];
                }
                for c in 0..k {
                    inv[r][c] ^= t[inv[col][c] as usize];
                }
            }
        }
    }
    inv
}

/// Matrix product (n x p) * (p x q) over GF(256).
fn mat_mul(a: &[Vec<u8>], b: &[Vec<u8>], n: usize, p: usize, q: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            (0..q)
                .map(|j| {
                    let mut acc = 0u8;
                    for t in 0..p {
                        acc ^= gf256::mul(a[i][t], b[t][j]);
                    }
                    acc
                })
                .collect()
        })
        .collect()
}

/// Solves a k x k GF(256) linear system a * x = b, where each unknown
/// `x[j]` is a byte-vector of length `len` (column-wise elimination:
/// one elimination pass applies to all `len` systems simultaneously).
fn solve_gf256(a: &mut [Vec<u8>], b: &mut [Vec<u8>], k: usize, _len: usize) -> Vec<Vec<u8>> {
    // rhs is mutated in place during elimination; it holds the solution
    // when a is the identity.
    let mut rhs: Vec<Vec<u8>> = b.to_vec();
    for col in 0..k {
        let pivot = (col..k)
            .find(|&r| a[r][col] != 0)
            .expect("singular shard system");
        a.swap(col, pivot);
        rhs.swap(col, pivot);
        let inv = gf256::inv(a[col][col]);
        if inv != 1 {
            let table = gf256::mul_table(inv);
            for v in a[col].iter_mut() {
                *v = table[*v as usize];
            }
            for byte in rhs[col].iter_mut() {
                *byte = table[*byte as usize];
            }
        }
        for r in 0..k {
            if r != col && a[r][col] != 0 {
                let factor = a[r][col];
                let table = gf256::mul_table(factor);
                for c in 0..k {
                    let sub = table[a[col][c] as usize];
                    a[r][c] ^= sub;
                }
                // Copy the pivot rhs row to end the rhs[col] borrow.
                let pivot_rhs = rhs[col].clone();
                for (rb, &cb) in rhs[r].iter_mut().zip(pivot_rhs.iter()) {
                    *rb ^= table[cb as usize];
                }
            }
        }
    }
    // a is now the identity: rhs holds the solution.
    let _ = rhs.len();
    rhs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards(k: usize, len: usize, seed: u64) -> Vec<Vec<u8>> {
        (0..k)
            .map(|i| {
                let mut s = seed.wrapping_add(i as u64);
                (0..len)
                    .map(|j| {
                        s = crate::io_engine::shard::splitmix64(s ^ (j as u64) << 32);
                        (s >> 56) as u8
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn systematic_matrix_shape() {
        let rs = RsCode::new(6, 4);
        let m = rs.encoding_matrix();
        assert_eq!(m.len(), 6);
        assert_eq!(m[0].len(), 4);
        // Top 4x4 is the identity.
        for (i, row) in m.iter().enumerate().take(4) {
            for (j, &v) in row.iter().enumerate() {
                assert_eq!(v, u8::from(i == j), "systematic block broken at ({i},{j})");
            }
        }
    }

    #[test]
    fn encode_data_shards_pass_through() {
        let rs = RsCode::new(5, 3);
        let data = shards(3, 256, 42);
        let encoded = rs.encode(&data);
        assert_eq!(encoded.len(), 5);
        assert_eq!(encoded[0], data[0]);
        assert_eq!(encoded[1], data[1]);
        assert_eq!(encoded[2], data[2]);
        assert_ne!(encoded[3], data[0]);
    }

    #[test]
    fn reconstruct_single_erasure() {
        let rs = RsCode::new(6, 4);
        let data = shards(4, 512, 7);
        let encoded = rs.encode(&data);
        // Lose shard 2.
        let surviving: Vec<(usize, Vec<u8>)> = encoded
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 2)
            .map(|(i, s)| (i, s.clone()))
            .collect();
        let rebuilt = rs.reconstruct(&surviving);
        assert_eq!(rebuilt[..4], data[..]);
        // And the re-encoded parity matches the original parity.
        assert_eq!(rebuilt[4], encoded[4]);
        assert_eq!(rebuilt[5], encoded[5]);
    }

    #[test]
    fn reconstruct_double_erasure() {
        let rs = RsCode::new(6, 4);
        let data = shards(4, 512, 11);
        let encoded = rs.encode(&data);
        // Lose shards 0 and 5 (one data, one parity).
        let surviving: Vec<(usize, Vec<u8>)> = encoded
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 0 && *i != 5)
            .map(|(i, s)| (i, s.clone()))
            .collect();
        let rebuilt = rs.reconstruct(&surviving);
        assert_eq!(rebuilt[..4], data[..]);
        assert_eq!(rebuilt[5], encoded[5]);
    }

    #[test]
    fn reconstruct_from_parity_only_plus_some_data() {
        // RS(10, 6): survive on any 6 of 10 -- here, one data shard and
        // five parity shards.
        let rs = RsCode::new(10, 6);
        let data = shards(6, 128, 99);
        let encoded = rs.encode(&data);
        let mut surviving: Vec<(usize, Vec<u8>)> = Vec::new();
        surviving.push((2, encoded[2].clone()));
        for i in 6..10 {
            surviving.push((i, encoded[i].clone()));
        }
        // Need k=6 shards: add one more data shard.
        surviving.push((3, encoded[3].clone()));
        let rebuilt = rs.reconstruct(&surviving);
        assert_eq!(rebuilt[..6], data[..]);
    }

    #[test]
    fn rs46_matches_raid6_class_single_loss() {
        // Interop sanity: single loss reconstruction restores exact data
        // for the RS(4, 2) minimal code (n-k=2 parity like RAID6).
        let rs = RsCode::new(4, 2);
        let data = shards(2, 1024, 3);
        let encoded = rs.encode(&data);
        for lost in 0..4 {
            let surviving: Vec<(usize, Vec<u8>)> = encoded
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != lost)
                .map(|(i, s)| (i, s.clone()))
                .collect();
            let rebuilt = rs.reconstruct(&surviving);
            assert_eq!(
                rebuilt[..2],
                data[..],
                "single loss of shard {lost} must recover"
            );
        }
    }

    #[test]
    fn random_erasure_property() {
        // The property the healer actually depends on: any k of n
        // shards recover the data. 200 rounds, random erasure sets --
        // the 1.x RAID6 equivalence-test discipline.
        let rs = RsCode::new(8, 5);
        let mut rng_state = 0xC0FFEEu64;
        for round in 0..200 {
            let data = shards(5, 64, rng_state.wrapping_add(round));
            let encoded = rs.encode(&data);
            // Random surviving set of exactly k shards.
            let mut indices: Vec<usize> = (0..8).collect();
            // Fisher-Yates with splitmix64.
            for i in (1..indices.len()).rev() {
                rng_state = crate::io_engine::shard::splitmix64(rng_state);
                let j = (rng_state % ((i + 1) as u64)) as usize;
                indices.swap(i, j);
            }
            let surviving: Vec<(usize, Vec<u8>)> = indices[..5]
                .iter()
                .map(|&i| (i, encoded[i].clone()))
                .collect();
            let rebuilt = rs.reconstruct(&surviving);
            assert_eq!(
                rebuilt[..5],
                data[..],
                "round {round}: surviving set {:?} failed to reconstruct",
                &indices[..5]
            );
        }
    }

    #[test]
    fn zero_data_encodes_and_reconstructs() {
        let rs = RsCode::new(4, 2);
        let data = vec![vec![0u8; 64], vec![0u8; 64]];
        let encoded = rs.encode(&data);
        // Parity of zeros is zeros.
        assert!(encoded[2].iter().all(|&b| b == 0));
        let surviving: Vec<(usize, Vec<u8>)> =
            vec![(1, encoded[1].clone()), (2, encoded[2].clone())];
        let rebuilt = rs.reconstruct(&surviving);
        assert_eq!(rebuilt[..2], data[..]);
    }

    #[test]
    fn geometry_facts() {
        let rs = RsCode::new(10, 6);
        assert_eq!(rs.parity(), 4);
        assert_eq!(rs.tolerates(), 4);
        // 6/10 storage efficiency.
        let efficiency = rs.k as f64 / rs.n as f64;
        assert!((efficiency - 0.6).abs() < 1e-9);
    }

    #[test]
    #[should_panic(expected = "at least one parity shard")]
    fn degenerate_codes_rejected() {
        let _ = RsCode::new(4, 4);
    }

    #[test]
    #[should_panic(expected = "at most 255")]
    fn gf256_shard_limit_enforced() {
        let _ = RsCode::new(256, 200);
    }
}
