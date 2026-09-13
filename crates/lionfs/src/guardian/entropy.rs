//! Ransomware / entropy watcher (RFC-004 §7.1).
//!
//! The signature Guardian looks for is the *rewrite-encrypt-everything*
//! pattern: a writer that (a) rewrites existing files at high rate,
//! (b) replaces low-entropy payloads with high-entropy ciphertext, and
//! (c) touches many files that share lure extensions (.docx, .xls,
//! .jpg -- the files ransomware monetizes). No single signal is proof;
//! the watcher keeps EWMA evidence per signal and fires an advisory
//! when the *combined* score crosses threshold.
//!
//! Shannon entropy per byte is computed over a 256-symbol histogram in
//! integer fixed-point (no floats), so the deterministic simulator and
//! the production daemon agree bit-for-bit. `log2` is decomposed into
//! an exact integer floor plus a 16-step quantized fractional part;
//! worst-case error is ~0.09 bits/byte, immaterial against the 8.0
//! ceiling and the 7.5 freeze line.

/// `log2(1 + j/16)` in 16.16 fixed point, `j = 0..=15`.
const FRACT16: [u32; 16] = [
    0, 5_732, 11_134, 16_241, 21_087, 25_706, 30_102, 34_303, 38_328, 42_186, 45_890, 49_458,
    52_896, 56_211, 59_417, 62_516,
];

/// `log2(x)` as `(floor, fractional-part-in-16.16)` for `x >= 1`.
fn log2_parts(x: u64) -> (u64, u32) {
    debug_assert!(x >= 1);
    let f = 63 - u64::from(x.leading_zeros()); // floor(log2 x)
    let base = 1u64 << f;
    let j = (((x - base) as u128 * 16) / base as u128).min(15) as usize;
    (f, FRACT16[j])
}

/// Entropy of `data` in bits per byte (0.0..=8.0), fixed-point 32.32.
///
/// Empty input is 0 (the convention that makes "an empty file looks
/// random" a non-issue).
#[must_use]
pub fn entropy_bits_per_byte(data: &[u8]) -> u64 {
    if data.is_empty() {
        return 0;
    }
    let mut hist = [0u32; 256];
    for &b in data {
        hist[b as usize] += 1;
    }
    let len = data.len() as u64;
    let (lf, lfr) = log2_parts(len);
    let log2_len = (lf << 16) + lfr as u64; // 16.16
    let mut acc: u128 = 0; // 32.32 accumulation of -p * log2(p)
    for &count in hist.iter() {
        if count == 0 {
            continue;
        }
        let c = u64::from(count);
        let (cf, cfr) = log2_parts(c);
        let log2_c = (cf << 16) + cfr as u64;
        // -log2(p) = log2(len) - log2(count), positive since c <= len.
        let neg_log = log2_len - log2_c;
        // p = c/len in 32.32.
        let p = (u128::from(c) << 32) / u128::from(len);
        // term = p * neg_log: 32.32 * 16.16, shift 16.
        acc += (p * u128::from(neg_log)) >> 16;
    }
    (acc as u64).min(8 << 32)
}

/// The freeze line (basis points of combined suspicion).
pub const FREEZE_BPS: u32 = 8_000;

/// Combined suspicion verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Suspicion {
    /// 0..=10_000 (basis points of 100%).
    pub score_bps: u32,
    /// Whether the freeze threshold tripped.
    pub freeze_recommended: bool,
}

/// File extensions ransomware monetizes (observed across incident
/// reports); a *weak* signal on its own.
pub const LURE_EXTENSIONS: [&str; 12] = [
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pdf", "jpg", "jpeg", "png", "csv", "db",
];

/// Whether a path ends with one of the lure extensions (case-insensitive,
/// ASCII only).
#[must_use]
pub fn is_lure_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    LURE_EXTENSIONS.iter().any(|ext| {
        lower.ends_with(ext)
            && lower.len() > ext.len()
            && lower.as_bytes()[lower.len() - ext.len() - 1] == b'.'
    })
}

/// EWMA-tracked evidence for one watched directory tree (or volume).
///
/// The agent loop calls [`EntropyWatcher::observe`] once per
/// observation window with the window's write classification; the
/// watcher keeps exponential moving averages of the three signals and
/// converts them into a combined suspicion score.
pub struct EntropyWatcher {
    /// EWMA of observed stream entropy, 32.32 bits/byte.
    entropy_ewma: u64,
    /// EWMA of rewrite fraction (rewrites / writes), 32.32.
    rewrite_ewma: u64,
    /// EWMA of lure-extension fraction, 32.32.
    lure_ewma: u64,
    /// EWMA weight for new evidence, 32.32.
    alpha: u64,
}

impl EntropyWatcher {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entropy_ewma: 0,
            rewrite_ewma: 0,
            lure_ewma: 0,
            alpha: (1 << 32) / 4, // 0.25
        }
    }

    /// Records one observation window: `writes` total writes, of which
    /// `rewrites` hit existing files and `lures` touched lure-extension
    /// paths; `sample` is a payload sample for entropy estimation.
    pub fn observe(&mut self, writes: u64, rewrites: u64, lures: u64, sample: &[u8]) {
        if writes == 0 {
            return;
        }
        let ent = entropy_bits_per_byte(sample);
        self.entropy_ewma = ewma(self.entropy_ewma, ent, self.alpha);
        let rewrite_ratio = (rewrites << 32) / writes;
        self.rewrite_ewma = ewma(self.rewrite_ewma, rewrite_ratio, self.alpha);
        let lure_ratio = (lures << 32) / writes;
        self.lure_ewma = ewma(self.lure_ewma, lure_ratio, self.alpha);
    }

    /// Combined suspicion score. Weights (RFC-004 §7.1): entropy 0.5,
    /// rewrite 0.3, lure 0.2. Entropy evidence saturates at 7.5
    /// bits/byte -- below the 8.0 ceiling where both encrypted *and*
    /// compressed data live, which is why the rewrite/lure signals
    /// carry the discriminative weight.
    #[must_use]
    pub fn suspicion(&self) -> Suspicion {
        let ent_full: u64 = (7 << 32) + (1 << 32) / 2; // 7.5 in 32.32
        let ent = self.entropy_ewma.min(ent_full) as u128;
        let ent_evidence = ((ent << 32) / u128::from(ent_full)) as u64; // 0..=1.0 in 32.32
        let combined = ent_evidence / 2
            + (self.rewrite_ewma / 10) * 3
            + (self.lure_ewma / 10) * 2;
        let score_bps = (((combined as u128) * 10_000) >> 32) as u32;
        let score_bps = score_bps.min(10_000);
        Suspicion {
            score_bps,
            freeze_recommended: score_bps >= FREEZE_BPS,
        }
    }

    /// Current EWMA evidence (diagnostics / exporter): (entropy,
    /// rewrite, lure), all 32.32.
    #[must_use]
    pub fn evidence(&self) -> (u64, u64, u64) {
        (self.entropy_ewma, self.rewrite_ewma, self.lure_ewma)
    }
}

impl Default for EntropyWatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// EWMA in 32.32: `new = (a*sample + (1-a)*old) >> 32`.
fn ewma(old: u64, sample: u64, alpha: u64) -> u64 {
    let a = alpha.min(1 << 32);
    let one_minus = (1 << 32) - a;
    let num = u128::from(sample) * u128::from(a) + u128::from(old) * u128::from(one_minus);
    (num >> 32) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_of_uniform_noise_is_maximal() {
        // A full 256-symbol cycle: exactly 8 bits/byte by construction.
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let bits = entropy_bits_per_byte(&data);
        assert_eq!(bits, 8 << 32);
    }

    #[test]
    fn entropy_of_constant_is_zero() {
        assert_eq!(entropy_bits_per_byte(b"aaaaaaaaaa"), 0);
        assert_eq!(entropy_bits_per_byte(&[]), 0);
        assert_eq!(entropy_bits_per_byte(&[7]), 0);
    }

    #[test]
    fn entropy_of_two_symbols_is_one_bit() {
        let data = vec![0u8, 1].repeat(512);
        assert_eq!(entropy_bits_per_byte(&data), 1 << 32);
    }

    #[test]
    fn entropy_of_four_symbols_is_two_bits() {
        let data: Vec<u8> = [0u8, 1, 2, 3].iter().cycle().copied().take(4096).collect();
        assert_eq!(entropy_bits_per_byte(&data), 2 << 32);
    }

    #[test]
    fn entropy_is_monotone_in_randomization() {
        let low = vec![b'a'; 1024];
        let mid: Vec<u8> = (0..1024u32).map(|i| (i % 4) as u8).collect();
        let high: Vec<u8> = (0..1024u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let e_low = entropy_bits_per_byte(&low);
        let e_mid = entropy_bits_per_byte(&mid);
        let e_high = entropy_bits_per_byte(&high);
        assert!(e_low < e_mid);
        assert!(e_mid < e_high);
    }

    #[test]
    fn entropy_of_english_text_is_midrange() {
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(64);
        let bits = entropy_bits_per_byte(&text);
        // English text sits around 3.5-4.5 bits/byte; the 16-step log2
        // quantization adds at most ~0.09.
        assert!(bits > (3 << 32), "bits {bits}");
        assert!(bits < (5 << 32), "bits {bits}");
    }

    #[test]
    fn normal_workload_stays_calm() {
        // Text-ish writes, mostly new files, no lure extensions.
        let mut w = EntropyWatcher::new();
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(20);
        for _ in 0..10 {
            w.observe(100, 10, 0, &text);
        }
        let s = w.suspicion();
        assert!(!s.freeze_recommended);
        // Entropy ~4/7.5 * 0.5 = ~0.27, rewrite 0.1*0.3 = 0.03: ~30%.
        assert!(s.score_bps < 4_000, "score {}", s.score_bps);
    }

    #[test]
    fn ransomware_pattern_freezes() {
        let mut w = EntropyWatcher::new();
        let ciphertext: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(0x9E37_79B1) >> 24) as u8)
            .collect();
        for _ in 0..12 {
            // All rewrites, all lure paths, all high-entropy payload.
            w.observe(100, 100, 100, &ciphertext);
        }
        let s = w.suspicion();
        assert!(s.freeze_recommended, "score {}", s.score_bps);
        assert!(s.score_bps >= 8_000, "score {}", s.score_bps);
    }

    #[test]
    fn compression_workload_does_not_freeze() {
        // Compressed output is high-entropy BUT: new files, no
        // rewrites, no lure extensions -> rewrite and lure stay 0, so
        // the score caps at the entropy weight alone (50%).
        let mut w = EntropyWatcher::new();
        let compressed: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(0x85EB_CA6B) >> 24) as u8)
            .collect();
        for _ in 0..12 {
            w.observe(100, 0, 0, &compressed);
        }
        let s = w.suspicion();
        assert!(!s.freeze_recommended);
        assert!(s.score_bps <= 5_000, "score {}", s.score_bps);
    }

    #[test]
    fn lure_paths_detected_case_insensitively() {
        assert!(is_lure_path("report.Q4.DOCX"));
        assert!(is_lure_path("/home/lion/photo.JPG"));
        assert!(is_lure_path("backup.db"));
        assert!(!is_lure_path("mydocx")); // no dot separator
        assert!(!is_lure_path("archive.tar.gz"));
        assert!(!is_lure_path("kernel.rs"));
    }

    #[test]
    fn ewma_converges() {
        let mut v = 0u64;
        let sample = 100u64 << 32;
        for _ in 0..60 {
            v = ewma(v, sample, (1 << 32) / 4);
        }
        // 0.75^60 ~ 1.8e-8: the residual gap is thousands, not millions.
        assert!((v as i64 - (100i64 << 32)).abs() < 1 << 20);
    }

    #[test]
    fn log2_parts_exactly_right_for_powers_of_two() {
        for k in 0..40u64 {
            let (f, fr) = log2_parts(1 << k);
            assert_eq!(f, k);
            assert_eq!(fr, 0);
        }
        let (f, fr) = log2_parts(3); // 1.58496 -> floor 1
        assert_eq!(f, 1);
        assert!(fr > 0 && fr < 1 << 16);
    }
}
