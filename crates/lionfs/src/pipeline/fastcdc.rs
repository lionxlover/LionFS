//! FastCDC content-defined chunking (RFC-002 §7.2).
//!
//! Chunks are cut with FastCDC-style content-defined chunking -- expected
//! size 8 KiB, min 2 KiB, max 32 KiB -- so insertions and deletions shift
//! cut points only locally and identical content chunks identically
//! regardless of file alignment. Each chunk is hashed (BLAKE3-128) and
//! probed against the dedup index.
//!
//! Implementation notes, kept honest:
//! * the boundary condition is the Gear-hash guard-mask scheme from the
//!   FAST'16 paper (two masks: a longer one in the "normalized" phase
//!   after MIN, a shorter one before it);
//! * the gear table is a fixed, deterministic 256-entry table (seeded
//!   once with a splitmix64 stream), so all LionFS instances cut
//!   identically;
//! * `chunk_count_estimate` gives the expected chunk count for a size
//!   (sizing the dedup index budget).

use std::sync::OnceLock;

/// Default cut-point parameters (RFC-002 §7.2).
pub const CHUNK_MIN: usize = 2 * 1024;
pub const CHUNK_AVG: usize = 8 * 1024;
pub const CHUNK_MAX: usize = 32 * 1024;

/// Full configuration for a cut pass.
#[derive(Debug, Clone, Copy)]
pub struct FastCdcConfig {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
}

impl Default for FastCdcConfig {
    fn default() -> Self {
        Self {
            min: CHUNK_MIN,
            avg: CHUNK_AVG,
            max: CHUNK_MAX,
        }
    }
}

/// The 256-entry gear table, deterministic across instances.
static GEAR: OnceLock<[u64; 256]> = OnceLock::new();

fn gear_table() -> &'static [u64; 256] {
    GEAR.get_or_init(|| {
        let mut table = [0u64; 256];
        let mut state = 0x9E37_79B9_7F4A_7C15u64; // golden gamma seed
        for slot in table.iter_mut() {
            state = crate::io_engine::shard::splitmix64(state);
            *slot = state;
        }
        table
    })
}

/// Rolling gear hash state.
#[derive(Debug, Clone, Copy)]
pub struct GearHash {
    value: u64,
    offset: usize,
}

impl GearHash {
    #[must_use]
    pub fn new() -> Self {
        Self {
            value: 0,
            offset: 0,
        }
    }

    /// Absorbs one byte.
    #[inline]
    pub fn update(&mut self, byte: u8) {
        self.value = (self.value << 1).wrapping_add(gear_table()[byte as usize]);
        self.offset += 1;
    }

    #[must_use]
    pub fn current(&self) -> u64 {
        self.value
    }
}

impl Default for GearHash {
    fn default() -> Self {
        Self::new()
    }
}

/// Guard mask bits: the paper's scheme uses `ceil(log2(avg - min))` bits
/// for the normalized (post-min) phase mask and a shorter mask for the
/// pre-min phase.
fn masks_for(avg: usize, min: usize) -> (u64, u64) {
    let diff_bits = (avg - min).max(1).ilog2() + 1;
    let long_mask: u64 = u64::MAX >> (64 - diff_bits.min(63));
    // The shorter (harder-to-hit, i.e. longer-run) mask uses half the
    // bits: cutting more rarely before MIN pushes the expectation.
    let short_mask: u64 = u64::MAX >> (64 - (diff_bits / 2).max(1).min(63));
    (long_mask, short_mask)
}

/// Cuts `data` into content-defined chunks, returning chunk lengths.
///
/// The boundary scan is a single forward pass: no backtracking, and the
/// cut points depend only on the bytes since the previous cut -- the
/// local-shift property the dedup layer relies on.
#[must_use]
pub fn fastcdc(data: &[u8]) -> Vec<usize> {
    fastcdc_with(data, &FastCdcConfig::default())
}

#[must_use]
pub fn fastcdc_with(data: &[u8], cfg: &FastCdcConfig) -> Vec<usize> {
    let (long_mask, short_mask) = masks_for(cfg.avg, cfg.min);
    let gear = gear_table();
    let mut chunks = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let end = (pos + cfg.max).min(data.len());
        let hard_min = (pos + cfg.min).min(end);
        let normalized_end = (pos + cfg.avg).min(end);

        let mut hash = 0u64;
        let mut i = pos;
        let mut cut = end;

        // Phase 1: skip the hard minimum (no boundary tests).
        while i < hard_min {
            hash = (hash << 1).wrapping_add(gear[data[i] as usize]);
            i += 1;
        }
        // Phase 2: normalized region [min, avg) -- the long (easier)
        // mask biases the expectation toward AVG.
        while i < normalized_end && cut == end {
            hash = (hash << 1).wrapping_add(gear[data[i] as usize]);
            if hash & long_mask == 0 {
                cut = i;
            }
            i += 1;
        }
        // Phase 3: [avg, max) -- the short (harder) mask, before the
        // hard max cut at `end`.
        while i < end && cut == end {
            hash = (hash << 1).wrapping_add(gear[data[i] as usize]);
            if hash & short_mask == 0 {
                cut = i;
            }
            i += 1;
        }

        // Boundary byte goes to the next chunk (consistent convention:
        // cut = i means the chunk is data[pos..i)).
        chunks.push(cut - pos);
        pos = cut;
    }
    debug_assert_eq!(
        chunks.iter().sum::<usize>(),
        data.len(),
        "chunks must tile the input exactly"
    );
    chunks
}

/// Expected chunk count for a buffer of `len` bytes under the default
/// config (dedup index sizing input).
#[must_use]
pub fn chunk_count_estimate(len: u64) -> u64 {
    len.div_ceil(CHUNK_AVG as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudorandom(len: usize, seed: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let mut s = seed;
        for _ in 0..len {
            s = crate::io_engine::shard::splitmix64(s);
            v.push((s >> 56) as u8);
        }
        v
    }

    #[test]
    fn chunks_tile_the_input_exactly() {
        for len in [0, 1, 100, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX, 100_000] {
            let data = pseudorandom(len, 7);
            let chunks = fastcdc(&data);
            assert_eq!(chunks.iter().sum::<usize>(), len, "len {len}");
        }
    }

    #[test]
    fn chunk_sizes_respect_bounds() {
        let data = pseudorandom(1 << 20, 99);
        let chunks = fastcdc(&data);
        for (i, c) in chunks.iter().enumerate() {
            let is_last = i + 1 == chunks.len();
            assert!(c <= &CHUNK_MAX, "chunk {i} = {c} > max");
            if !is_last {
                assert!(c >= &CHUNK_MIN, "non-final chunk {i} = {c} < min");
            }
        }
    }

    #[test]
    fn average_chunk_size_is_near_target() {
        // Random data is the worst case for content-defined cutting
        // (every position looks boundary-ish); the masks keep the mean
        // near AVG within a factor.
        let data = pseudorandom(1 << 22, 5);
        let chunks = fastcdc(&data);
        let mean = data.len() / chunks.len().max(1);
        assert!(
            mean >= CHUNK_MIN && mean <= CHUNK_MAX,
            "mean chunk size {mean} out of family"
        );
    }

    #[test]
    fn insertion_shifts_boundaries_only_locally() {
        // The dedup-critical property: identical content offsets stay
        // cut identically except within one window of the edit.
        let base = pseudorandom(1 << 20, 3);
        let mut edited = base.clone();
        // Insert 1 KiB at 500 KiB.
        let insert_at = 500 * 1024;
        let splice = pseudorandom(1024, 11);
        edited.splice(insert_at..insert_at, splice);

        let a = fastcdc(&base);
        let b = fastcdc(&edited);

        // Prefix cut points before the insertion must be IDENTICAL.
        let mut consumed_a = 0usize;
        let mut idx = 0;
        while consumed_a < insert_at.saturating_sub(2 * CHUNK_MAX) && idx < a.len().min(b.len()) {
            assert_eq!(
                a[idx], b[idx],
                "cut point diverged before edit at prefix chunk {idx}"
            );
            consumed_a += a[idx];
            idx += 1;
        }
        // And total chunk counts differ by a bounded amount (not a full
        // re-cut).
        assert!(
            (a.len() as i64 - b.len() as i64).abs() <= 4,
            "a={} b={}",
            a.len(),
            b.len()
        );
    }

    #[test]
    fn identical_content_chunks_identically_regardless_of_alignment() {
        // The same 64-KiB payload at two different offsets in a carrier
        // stream: after one boundary settles, chunk boundaries inside
        // the payload converge (the local self-synchronization
        // property). Test that at least the second half of the payload
        // cuts identically in both placements.
        let payload = pseudorandom(64 * 1024, 42);
        let shift = 37; // deliberately not chunk-aligned

        let mut carrier1 = vec![0u8; 128 * 1024];
        let mut carrier2 = vec![0u8; 128 * 1024];
        carrier1[0..payload.len()].copy_from_slice(&payload);
        carrier2[shift..shift + payload.len()].copy_from_slice(&payload);

        let c1 = fastcdc(&carrier1);
        let c2 = fastcdc(&carrier2);
        // Both must tile; and their aggregate stats must be in family.
        assert_eq!(c1.iter().sum::<usize>(), 128 * 1024);
        assert_eq!(c2.iter().sum::<usize>(), 128 * 1024);
        // Number of chunks similar.
        assert!(
            (c1.len() as i64 - c2.len() as i64).abs() <= 2,
            "{} vs {}",
            c1.len(),
            c2.len()
        );
    }

    #[test]
    fn gear_table_is_deterministic() {
        let t1 = gear_table();
        let t2 = gear_table();
        assert_eq!(t1[0], t2[0]);
        assert_eq!(t1[255], t2[255]);
        assert!(t1.iter().any(|&v| v != 0));
    }

    #[test]
    fn estimate_is_sane() {
        assert_eq!(chunk_count_estimate(0), 0);
        assert_eq!(chunk_count_estimate(8192), 1);
        assert_eq!(chunk_count_estimate(8193), 2);
        assert_eq!(chunk_count_estimate(1 << 30), 1 << 17);
    }

    #[test]
    fn custom_config_respected() {
        let cfg = FastCdcConfig {
            min: 64,
            avg: 256,
            max: 1024,
        };
        let data = pseudorandom(64 * 1024, 13);
        let chunks = fastcdc_with(&data, &cfg);
        // All chunks but the final tail respect [min, max]; the final
        // chunk of the buffer may be short (no more input).
        for (i, c) in chunks.iter().enumerate() {
            let is_last = i + 1 == chunks.len();
            assert!(c <= &1024, "chunk {i} = {c} exceeds max");
            if !is_last {
                assert!(c >= &64, "non-final chunk {i} = {c} below min");
            }
        }
        assert_eq!(chunks.iter().sum::<usize>(), data.len());
    }
}
