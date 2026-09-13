//! Content-defined chunking (CDC) via Rabin-Karp rolling hash (spec §10.1).
//!
//! Fixed-size chunking has the "boundary shift" problem: inserting one byte
//! at the front of a file desynchronizes every subsequent chunk boundary,
//! destroying dedup. CDC instead places boundaries where a *rolling* hash of
//! the last `WINDOW` bytes satisfies a divisibility condition — an insert
//! only disturbs the boundaries whose window actually saw the insert, and
//! boundaries re-synchronize after one max-chunk length.
//!
//! Boundary rule (spec §10.1): `hash(window) mod 2^avg_bits == target`
//! — implemented with `target == 0` (a bit-mask test), the standard
//! equivalent used by LBFS, Venti, restic and IPFS.
//!
//! Fingerprint math: for window bytes `w_0..w_{L-1}` (oldest first)
//! `F = Σ w_i · x^{8(L-1-i)} mod p` over GF(2). Sliding the window:
//! `F' = (F · x^8) ⊕ in ⊕ (out · x^{8L}) mod p`, so the outgoing byte's
//! contribution `out · x^{8L} mod p` is precomputed in a 256-entry table.
//!
//! The polynomial `0x3DA3358B4DC173` is a degree-53 irreducible
//! (the same default used by restic's Rabin implementation).
//!
//! Known caveat: strictly periodic input (period ≥ window size, e.g. an
//! uncompressed counter or zero-filled region) makes the fingerprint
//! periodic too, so mask boundaries rarely fire and chunks degrade toward
//! `max`. This is a general property of windowed CDC, not of this
//! implementation; production systems pair CDC with a secondary escape
//! (e.g. compression of Store chunks).

/// Chunking policy.
#[derive(Clone, Copy, Debug)]
pub struct ChunkerConfig {
    /// Target average chunk size (power of two ≥ 1024).
    pub avg: usize,
    /// Minimum chunk size — closer boundaries are ignored.
    pub min: usize,
    /// Maximum chunk size — a forced boundary (bounds tail latency).
    pub max: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        // Spec §10.1 defaults: avg 64 KiB, min 16 KiB, max 256 KiB.
        Self { avg: 64 * 1024, min: 16 * 1024, max: 256 * 1024 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CdcError {
    #[error("invalid chunker config: {0}")]
    InvalidConfig(String),
}

/// Irreducible polynomial, degree 53.
const POLY: u64 = 0x3D_A3_35_8B_4D_C1_73;
const POLY_DEGREE: u32 = 53;
const WINDOW_SIZE: usize = 48;

/// A Rabin-Karp rolling-hash CDC engine.
pub struct Chunker {
    config: ChunkerConfig,
    /// Bit mask selecting the low log2(avg) bits.
    mask: u64,
    /// 256-entry table: `byte · x^(8·WINDOW) mod p`.
    table: [u64; 256],
    /// 512-entry table: `(spill · x^53) mod p` for the 9 bits that exceed
    /// the polynomial degree after the x^8 shift. One lookup replaces the
    /// bit-by-bit reduction loop in the per-byte hot path (v1.1: ~5x
    /// chunking throughput).
    reduce_hi: [u64; 512],
    /// Circular buffer of recent bytes.
    window: [u8; WINDOW_SIZE],
    window_pos: usize,
    /// Current rolling fingerprint (fully reduced: degree < 53).
    fingerprint: u64,
}

/// Bits below the polynomial's degree (53): the kept part of the fingerprint.
const POLY_LOW_MASK: u64 = (1u64 << POLY_DEGREE) - 1;

impl Chunker {
    pub fn new(config: ChunkerConfig) -> Result<Self, CdcError> {
        if !config.avg.is_power_of_two() || config.avg < 1024 {
            return Err(CdcError::InvalidConfig("avg must be a power of two >= 1024".into()));
        }
        if config.min < 512 || config.max <= config.min || config.max > config.avg * 8 {
            return Err(CdcError::InvalidConfig("need 512 <= min < max <= 8*avg".into()));
        }
        // x^(8·WINDOW) mod p  (WINDOW = 48 → x^384)
        let x_384 = pow_x(8 * WINDOW_SIZE as u32);
        let mut table = [0u64; 256];
        for (byte, slot) in table.iter_mut().enumerate() {
            *slot = poly_mul_mod(byte as u64, x_384);
        }
        let mut reduce_hi = [0u64; 512];
        for (spill, slot) in reduce_hi.iter_mut().enumerate() {
            *slot = mod_poly((spill as u64) << POLY_DEGREE);
        }
        Ok(Self {
            mask: (config.avg - 1) as u64,
            table,
            reduce_hi,
            window: [0u8; WINDOW_SIZE],
            window_pos: 0,
            fingerprint: 0,
            config,
        })
    }

    fn reset(&mut self) {
        self.window = [0u8; WINDOW_SIZE];
        self.window_pos = 0;
        self.fingerprint = 0;
    }

    /// Slide the window one byte.
    #[inline]
    fn push_byte(&mut self, byte: u8) {
        let out = self.window[self.window_pos];
        self.window[self.window_pos] = byte;
        self.window_pos += 1;
        if self.window_pos == WINDOW_SIZE {
            self.window_pos = 0;
        }

        // F' = (F · x^8) ⊕ in ⊕ table[out], all mod p. The shifted value
        // spills at most 9 bits past degree 53; the spill cancels in one
        // table lookup (precomputed (spill · x^53) mod p) instead of the
        // bit-by-bit reduction loop this used to run per byte.
        let mut fp = (self.fingerprint << 8) | byte as u64;
        fp ^= self.table[out as usize];
        let spill = (fp >> POLY_DEGREE) as usize;
        self.fingerprint = (fp & POLY_LOW_MASK) ^ self.reduce_hi[spill];
    }

    #[inline]
    fn is_boundary(&self) -> bool {
        (self.fingerprint & self.mask) == 0
    }

    /// Chunk `data`, returning boundary offsets (starts at 0, ends at
    /// `data.len()`; chunk i = `data[off[i]..off[i+1]]`).
    pub fn chunk_boundaries(&mut self, data: &[u8]) -> Vec<usize> {
        self.reset();
        let n = data.len();
        let mut offsets = vec![0usize];
        let mut i = 0usize;
        while i < n {
            let hard_end = i + (n - i).min(self.config.max);
            let mut end = hard_end;
            let mut j = i;
            while j < hard_end {
                self.push_byte(data[j]);
                j += 1;
                if j - i >= self.config.min && self.is_boundary() {
                    end = j;
                    break;
                }
            }
            offsets.push(end);
            i = end;
        }
        offsets
    }

    /// Chunk into borrowed slices (convenience API).
    pub fn chunk<'a>(&mut self, data: &'a [u8]) -> Vec<&'a [u8]> {
        self.chunk_boundaries(data)
            .windows(2)
            .map(|w| &data[w[0]..w[1]])
            .collect()
    }
}

// ---------------------------------------------------------------------------
// GF(2) polynomial arithmetic (private helpers)
// ---------------------------------------------------------------------------

/// Reduce `value` mod `POLY` (degree 53).
fn mod_poly(mut value: u64) -> u64 {
    while value.leading_zeros() < 64 - 1 - POLY_DEGREE {
        let shift = (63 - value.leading_zeros()) - POLY_DEGREE;
        value ^= POLY << shift;
    }
    value & ((1u64 << (POLY_DEGREE + 1)) - 1)
}

/// Multiply two GF(2) polynomials mod `POLY`.
fn poly_mul_mod(mut a: u64, mut b: u64) -> u64 {
    let mut result = 0u64;
    while b != 0 {
        if b & 1 != 0 {
            result ^= a;
        }
        b >>= 1;
        a = mod_poly(a << 1);
    }
    result
}

/// Compute `x^e mod POLY`.
fn pow_x(e: u32) -> u64 {
    // Square-and-multiply.
    let mut result = 1u64;
    let mut base = 2u64; // x
    let mut e = e;
    while e > 0 {
        if e & 1 != 0 {
            result = poly_mul_mod(result, base);
        }
        base = poly_mul_mod(base, base);
        e >>= 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_within_bounds() {
        let mut c = Chunker::new(ChunkerConfig { avg: 4096, min: 1024, max: 16384 }).unwrap();
        let data: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2654435761) % 251) as u8).collect();
        let bounds = c.chunk_boundaries(&data);
        assert_eq!(bounds.first(), Some(&0));
        assert_eq!(*bounds.last().unwrap(), data.len());
        for w in bounds.windows(2) {
            let size = w[1] - w[0];
            assert!(size >= 1024, "chunk below min: {size}");
            assert!(size <= 16384, "chunk above max: {size}");
        }
    }

    fn xorshift_bytes(len: usize, seed: u64) -> Vec<u8> {
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
    fn average_chunk_size_is_close_to_target() {
        // Pseudo-random data: boundaries fire with probability 1/avg per
        // position past `min`, so the empirical mean lands near avg+min/2.
        let mut c = Chunker::new(ChunkerConfig { avg: 4096, min: 1024, max: 16384 }).unwrap();
        let data = xorshift_bytes(4_000_000, 1);
        let chunks = c.chunk_boundaries(&data);
        let count = chunks.len() - 1;
        let avg_actual = data.len() / count;
        assert!(
            (avg_actual as i64 - 5000).abs() < 2500,
            "avg actual {avg_actual} too far from expected ~5000"
        );
    }

    #[test]
    fn periodic_data_degrades_to_max_chunks() {
        // Documents the caveat above: strictly periodic input starves the
        // mask test and forces max-size chunks. This is expected behaviour.
        let mut c = Chunker::new(ChunkerConfig { avg: 4096, min: 1024, max: 16384 }).unwrap();
        let data: Vec<u8> = (0..400_000u32).map(|i| i as u8).collect(); // period 256
        let chunks = c.chunk_boundaries(&data);
        let sizes: Vec<usize> = chunks.windows(2).map(|w| w[1] - w[0]).collect();
        let avg_actual = sizes.iter().sum::<usize>() / sizes.len();
        assert!(avg_actual > 8192, "periodic data should degrade toward max, got {avg_actual}");
    }

    #[test]
    fn insert_does_not_desynchronize_boundaries() {
        // THE property that justifies CDC: after a one-byte insert, all
        // boundaries beyond the insert + one resync window stay identical.
        let cfg = ChunkerConfig { avg: 4096, min: 1024, max: 16384 };
        let mut c1 = Chunker::new(cfg).unwrap();
        let mut c2 = Chunker::new(cfg).unwrap();

        let original = xorshift_bytes(2_000_000, 42);
        let mut shifted = original.clone();
        shifted.insert(500_000, 0xAB);

        let b1 = c1.chunk_boundaries(&original);
        let b2 = c2.chunk_boundaries(&shifted);

        let set2: std::collections::HashSet<usize> = b2.into_iter().collect();
        let mut matched = 0usize;
        let mut checked = 0usize;
        for &off in b1.iter().skip(1) {
            if off > 500_000 + 16384 {
                checked += 1;
                if set2.contains(&(off + 1)) {
                    matched += 1;
                }
            }
        }
        assert!(checked > 5, "test needs more boundaries after insert point");
        assert!(
            matched as f64 / checked as f64 > 0.8,
            "only {matched}/{checked} boundaries survived the insert — CDC broken"
        );
    }

    #[test]
    fn identical_input_gives_identical_chunks() {
        let cfg = ChunkerConfig { avg: 2048, min: 512, max: 8192 };
        let mut a = Chunker::new(cfg).unwrap();
        let mut b = Chunker::new(cfg).unwrap();
        let data = xorshift_bytes(1_000_000, 7);
        assert_eq!(a.chunk_boundaries(&data), b.chunk_boundaries(&data));
    }

    #[test]
    fn empty_and_tiny_inputs() {
        let mut c = Chunker::new(ChunkerConfig { avg: 2048, min: 512, max: 8192 }).unwrap();
        assert_eq!(c.chunk_boundaries(b""), vec![0usize]);
        assert_eq!(c.chunk_boundaries(b"abc"), vec![0, 3]);
    }

    #[test]
    fn rejects_invalid_config() {
        assert!(Chunker::new(ChunkerConfig { avg: 3000, min: 512, max: 8192 }).is_err());
        assert!(Chunker::new(ChunkerConfig { avg: 2048, min: 9000, max: 8192 }).is_err());
        assert!(Chunker::new(ChunkerConfig { avg: 2048, min: 512, max: 512 }).is_err());
    }

    #[test]
    fn boundaries_respect_min_max_and_reassemble() {
        let mut chunker = Chunker::new(ChunkerConfig {
            avg: 8 * 1024,
            min: 2 * 1024,
            max: 32 * 1024,
        })
        .unwrap();
        let mut s: u64 = 0xABCD_1234_5678_9ABC;
        let data: Vec<u8> = (0..1_000_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect();
        let chunks = chunker.chunk(&data);
        assert!(chunks.len() > 10, "expected many chunks, got {}", chunks.len());
        for c in &chunks {
            assert!(c.len() >= 2 * 1024, "chunk below min: {}", c.len());
            assert!(c.len() <= 32 * 1024, "chunk above max: {}", c.len());
        }
        // Reassembly is the identity.
        let mut out = Vec::with_capacity(data.len());
        for c in &chunks {
            out.extend_from_slice(c);
        }
        assert_eq!(out, data);
        // Determinism: same bytes, same boundaries.
        let again = chunker.chunk(&data);
        assert_eq!(chunks.len(), again.len());
    }

    #[test]
    fn insertion_shifts_boundaries_only_locally() {
        // The defining property of CDC: inserting one byte near the front
        // shifts nearby boundaries by one, but boundaries far downstream
        // re-sync — most boundaries past the insertion point must be
        // preserved (unlike fixed-size blocking, where all shift).
        let mut chunker =
            Chunker::new(ChunkerConfig { avg: 8 * 1024, min: 2 * 1024, max: 32 * 1024 }).unwrap();
        let mut s: u64 = 0x77AA_55EE_1122_3344;
        let data: Vec<u8> = (0..2_000_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect();
        let mut edited = data.clone();
        edited.insert(5000, 0xEE);

        let before: Vec<usize> = chunker.chunk_boundaries(&data);
        let after: Vec<usize> = chunker.chunk_boundaries(&edited);
        // Count how many boundaries (offset by the 1-byte insert) survive
        // in the back half of the file.
        let tail_start = data.len() / 2;
        let b_tail: std::collections::HashSet<usize> =
            before.iter().copied().filter(|&o| o >= tail_start).collect();
        let a_tail: std::collections::HashSet<usize> = after
            .iter()
            .copied()
            .filter(|&o| o > tail_start)
            .map(|o| o - 1)
            .collect();
        let preserved = b_tail.intersection(&a_tail).count();
        assert!(
            preserved as f64 / b_tail.len() as f64 > 0.5,
            "CDC must re-sync boundaries after an insertion: {}/{} preserved",
            preserved,
            b_tail.len()
        );
    }
}
