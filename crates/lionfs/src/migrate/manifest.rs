//! Migration manifest & verification (RFC-004 §9.2).
//!
//! A migration is a protocol, not a copy: every file the import
//! touches gets a ledger entry (path, size, SHA-256); the import is
//! not "done" until every entry re-verifies against the destination.
//! The manifest is the durable artifact of that protocol -- written
//! alongside the import, verified after, and kept for the operator's
//! audit trail.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

/// One ledger row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Source-relative POSIX path.
    pub path: String,
    /// Size in bytes at capture time.
    pub size: u64,
    /// SHA-256 of the content at capture time.
    pub digest: [u8; 32],
}

/// The ledger. Entries are appended in walk order; paths are unique
/// (appending a duplicate path is a bug and is refused).
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    entries: Vec<ManifestEntry>,
    index: HashMap<String, usize>,
}

impl Manifest {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a capture record for one file. `None` (refused) on
    /// duplicate paths.
    pub fn record(&mut self, path: &str, size: u64, digest: [u8; 32]) -> Option<()> {
        if self.index.contains_key(path) {
            return None;
        }
        self.index.insert(path.to_owned(), self.entries.len());
        self.entries.push(ManifestEntry {
            path: path.to_owned(),
            size,
            digest,
        });
        Some(())
    }

    /// Convenience: captures a file's content directly.
    pub fn record_content(&mut self, path: &str, content: &[u8]) -> Option<()> {
        let digest = sha256(content);
        self.record(path, content.len() as u64, digest)
    }

    /// Entry lookup by path.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&ManifestEntry> {
        self.index.get(path).map(|&i| &self.entries[i])
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All entries, in record order.
    #[must_use]
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// Total bytes accounted.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// Verifies the manifest against observed (path, size, content)
    /// pairs from the destination. Returns per-entry outcomes and a
    /// summary; the import reports success only when
    /// `summary.failures == 0 && summary.checked == len`.
    pub fn verify<'a, I>(&self, observed: I) -> VerifySummary
    where
        I: Iterator<Item = (&'a str, u64, &'a [u8])>,
    {
        let mut failures = Vec::new();
        let mut checked = 0usize;
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (path, size, content) in observed {
            seen.insert(path);
            checked += 1;
            match self.get(path) {
                None => failures.push(VerifyOutcome {
                    path: path.to_owned(),
                    kind: VerifyFailure::NotInManifest,
                }),
                Some(e) => {
                    if e.size != size {
                        failures.push(VerifyOutcome {
                            path: path.to_owned(),
                            kind: VerifyFailure::SizeMismatch { expected: e.size, got: size },
                        });
                    } else if sha256(content) != e.digest {
                        failures.push(VerifyOutcome {
                            path: path.to_owned(),
                            kind: VerifyFailure::DigestMismatch,
                        });
                    }
                }
            }
        }
        // Manifest entries never observed on the destination.
        for e in &self.entries {
            if !seen.contains(e.path.as_str()) {
                failures.push(VerifyOutcome {
                    path: e.path.clone(),
                    kind: VerifyFailure::Missing,
                });
            }
        }
        VerifySummary {
            entries: self.entries.len(),
            checked,
            failures,
        }
    }
}

/// Why one path failed verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyFailure {
    /// Observed on the destination but never captured.
    NotInManifest,
    /// Captured but never observed (import incomplete).
    Missing,
    SizeMismatch { expected: u64, got: u64 },
    /// Same size, different bits.
    DigestMismatch,
}

/// One failed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyOutcome {
    pub path: String,
    pub kind: VerifyFailure,
}

/// Verification roll-up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifySummary {
    /// Entries in the manifest.
    pub entries: usize,
    /// Paths actually checked.
    pub checked: usize,
    /// Every way the verification failed.
    pub failures: Vec<VerifyOutcome>,
}

impl VerifySummary {
    /// The import protocol's success condition.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty() && self.checked == self.entries && self.entries > 0
    }
}

/// SHA-256 of `data` (the manifest's digest choice: collision
/// resistance matters more than speed here, and sha2 is already in
/// the dependency set).
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_verifies() {
        let mut m = Manifest::new();
        assert!(m.record_content("docs/rfc.txt", b"LionFS RFC body").is_some());
        assert!(m.record_content("img/logo.bin", &[0u8; 1024]).is_some());
        assert_eq!(m.len(), 2);
        assert_eq!(m.total_bytes(), 15 + 1024);

        let obs = vec![
            ("docs/rfc.txt".to_owned(), 15, b"LionFS RFC body".to_vec()),
            ("img/logo.bin".to_owned(), 1024u64, vec![0u8; 1024]),
        ];
        let summary = m.verify(obs.iter().map(|(p, s, c)| (p.as_str(), *s, c.as_slice())));
        assert!(summary.is_complete());
        assert_eq!(summary.checked, 2);
    }

    #[test]
    fn duplicate_paths_are_refused() {
        let mut m = Manifest::new();
        assert!(m.record_content("a", b"1").is_some());
        assert!(m.record_content("a", b"2").is_none());
        assert_eq!(m.len(), 1);
        // The original entry is intact.
        assert_eq!(m.get("a").map(|e| e.size), Some(1));
    }

    #[test]
    fn corrupted_content_fails_digest() {
        let mut m = Manifest::new();
        m.record_content("f", b"original").expect("record");
        let mut obs = vec![("f".to_owned(), 8, b"original".to_vec())];
        let summary = m.verify(obs.iter().map(|(p, s, c)| (p.as_str(), *s, c.as_slice())));
        assert!(summary.is_complete());
        obs[0].2 = b"corrupt!".to_vec();
        let summary = m.verify(obs.iter().map(|(p, s, c)| (p.as_str(), *s, c.as_slice())));
        assert!(!summary.is_complete());
        assert_eq!(summary.failures[0].kind, VerifyFailure::DigestMismatch);
    }

    #[test]
    fn size_mismatch_is_reported_with_both_sides() {
        let mut m = Manifest::new();
        m.record_content("f", b"0123456789").expect("record");
        let obs = vec![("f".to_owned(), 9u64, b"012345678".to_vec())];
        let summary = m.verify(obs.iter().map(|(p, s, c)| (p.as_str(), *s, c.as_slice())));
        assert_eq!(
            summary.failures[0].kind,
            VerifyFailure::SizeMismatch { expected: 10, got: 9 }
        );
    }

    #[test]
    fn missing_destination_file_is_a_failure() {
        let mut m = Manifest::new();
        m.record_content("a", b"1").expect("record");
        m.record_content("b", b"2").expect("record");
        let obs = vec![("a".to_owned(), 1, b"1".to_vec())];
        let summary = m.verify(obs.iter().map(|(p, s, c)| (p.as_str(), *s, c.as_slice())));
        assert!(!summary.is_complete());
        assert!(summary
            .failures
            .iter()
            .any(|f| f.path == "b" && f.kind == VerifyFailure::Missing));
    }

    #[test]
    fn extra_destination_file_is_a_failure() {
        let mut m = Manifest::new();
        m.record_content("a", b"1").expect("record");
        let obs = vec![
            ("a".to_owned(), 1, b"1".to_vec()),
            ("ghost".to_owned(), 2, b"??".to_vec()),
        ];
        let summary = m.verify(obs.iter().map(|(p, s, c)| (p.as_str(), *s, c.as_slice())));
        assert!(!summary.is_complete());
        assert!(summary
            .failures
            .iter()
            .any(|f| f.path == "ghost" && f.kind == VerifyFailure::NotInManifest));
    }

    #[test]
    fn empty_manifest_is_never_complete() {
        let m = Manifest::new();
        let summary = m.verify(std::iter::empty());
        assert!(!summary.is_complete());
        assert!(m.is_empty());
    }

    #[test]
    fn sha256_known_answer() {
        // SHA-256("abc") -- the standard test vector.
        let d = sha256(b"abc");
        assert_eq!(
            hex(&d),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
