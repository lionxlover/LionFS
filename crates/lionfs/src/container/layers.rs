//! Image-layer content-addressable storage (RFC-004 §10.1).
//!
//! A layer is a (digest, byte-length, extent-count) triple registered
//! by the container runtime at pull/build time. The registry:
//!
//! * dedups by digest: pulling a layer that already exists is a
//!   refcount bump, not a re-download/re-write;
//! * shares extents at file granularity (via the pipeline's chunk
//!   dedup) and at *subtree* granularity via clone links;
//! * pins cloned-from subtrees in the hot dedup index so the sharing
//!   actually hits (cold-index chunks miss on pull);
//! * garbage-collects unreferenced layers when the last container
//!   releases them (the refcount drop rides the ordinary reclamation
//!   path -- RFC-004 §6).
//!
//! Digests are opaque 32-byte blobs here (runtimes use sha256; the
//! registry does not care and does not recompute them).

use std::collections::HashMap;

/// A layer's content digest (opaque; runtimes pass sha256).
pub type LayerDigest = [u8; 32];

/// What a layer registration provides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerSpec {
    /// Content digest (registry key).
    pub digest: LayerDigest,
    /// Uncompressed layer bytes.
    pub uncompressed_bytes: u64,
    /// Number of extents the layer materializes as (post-pipeline).
    pub extent_count: u32,
    /// Human-readable provenance ("docker.io/library/nginx:1.25").
    pub provenance: String,
}

impl LayerSpec {
    #[must_use]
    pub fn new(provenance: &str, uncompressed_bytes: u64, extent_count: u32) -> Self {
        // Digests are runtime-supplied; this constructor derives a
        // placeholder from the provenance so tests and callers that
        // don't track real digests still get distinct keys.
        let mut d = LayerDigest::default();
        for (i, b) in provenance.bytes().enumerate() {
            d[i % 32] ^= b.wrapping_mul((i + 1) as u8).wrapping_add(7);
        }
        Self {
            digest: d,
            uncompressed_bytes,
            extent_count,
            provenance: provenance.to_owned(),
        }
    }

    #[must_use]
    pub fn with_digest(mut self, digest: LayerDigest) -> Self {
        self.digest = digest;
        self
    }
}

/// One registered layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerEntry {
    pub spec: LayerSpec,
    /// How many containers/runtime handles reference this layer.
    pub refs: u32,
    /// Whether this layer's chunks are pinned in the hot dedup index.
    pub pinned: bool,
}

/// The registry: digest -> entry.
pub struct LayerRegistry {
    layers: HashMap<LayerDigest, LayerEntry>,
    /// Bytes saved by dedup on re-registration (observability).
    saved_bytes: u64,
}

impl LayerRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            layers: HashMap::new(),
            saved_bytes: 0,
        }
    }

    /// Registers (or re-references) a layer. Returns the entry and
    /// whether it was newly materialized. New layers pin their chunks
    /// in the hot dedup index; re-references just bump the count.
    pub fn register(&mut self, spec: LayerSpec) -> (LayerEntry, bool) {
        match self.layers.get_mut(&spec.digest) {
            Some(e) => {
                e.refs = e.refs.saturating_add(1);
                self.saved_bytes += spec.uncompressed_bytes;
                (e.clone(), false)
            }
            None => {
                let entry = LayerEntry {
                    spec,
                    refs: 1,
                    pinned: true,
                };
                self.layers.insert(entry.spec.digest, entry.clone());
                (entry, true)
            }
        }
    }

    /// Releases one reference. Returns `Some(true)` when the layer
    /// became unreferenced (the caller then schedules reclamation --
    /// the physical extent release rides the GC path).
    pub fn release(&mut self, digest: &LayerDigest) -> Option<bool> {
        let e = self.layers.get_mut(digest)?;
        e.refs = e.refs.saturating_sub(1);
        Some(e.refs == 0)
    }

    /// Lookup.
    #[must_use]
    pub fn get(&self, digest: &LayerDigest) -> Option<&LayerEntry> {
        self.layers.get(digest)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Total materialized bytes (one copy per unique layer).
    #[must_use]
    pub fn materialized_bytes(&self) -> u64 {
        self.layers.values().map(|e| e.spec.uncompressed_bytes).sum()
    }

    /// Total logical bytes (every reference counted).
    #[must_use]
    pub fn logical_bytes(&self) -> u64 {
        self.layers
            .values()
            .map(|e| e.spec.uncompressed_bytes.saturating_mul(u64::from(e.refs)))
            .sum()
    }

    /// Dedup savings so far (re-registered bytes).
    #[must_use]
    pub fn saved_bytes(&self) -> u64 {
        self.saved_bytes
    }

    /// The dedup ratio in basis points (logical / materialized):
    /// 10_000 = no sharing; 50_000 = 5x sharing.
    #[must_use]
    pub fn sharing_bps(&self) -> u64 {
        let mat = self.materialized_bytes();
        if mat == 0 {
            return 10_000;
        }
        (self.logical_bytes().saturating_mul(10_000)) / mat
    }

    /// Whether a layer's chunks are pinned (hot dedup index).
    #[must_use]
    pub fn is_pinned(&self, digest: &LayerDigest) -> bool {
        self.layers.get(digest).map(|e| e.pinned).unwrap_or(false)
    }

    /// Drops unreferenced layers from the registry (after the GC has
    /// reclaimed their extents). Returns the dropped digests.
    pub fn sweep(&mut self) -> Vec<LayerDigest> {
        let dead: Vec<LayerDigest> = self
            .layers
            .iter()
            .filter(|(_, e)| e.refs == 0)
            .map(|(d, _)| *d)
            .collect();
        for d in &dead {
            self.layers.remove(d);
        }
        dead
    }
}

impl Default for LayerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, bytes: u64) -> LayerSpec {
        LayerSpec::new(name, bytes, 8)
    }

    #[test]
    fn distinct_provenances_get_distinct_digests() {
        let a = spec("nginx:1.25", 100);
        let b = spec("redis:7", 100);
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn registration_dedups_by_digest() {
        let mut r = LayerRegistry::new();
        let s = spec("ubuntu:24.04", 100_000_000);
        let (e1, new1) = r.register(s.clone());
        assert!(new1);
        assert_eq!(e1.refs, 1);
        assert!(r.is_pinned(&s.digest));

        // Same digest: a refcount bump, no re-materialization.
        let (e2, new2) = r.register(s.clone());
        assert!(!new2);
        assert_eq!(e2.refs, 2);
        assert_eq!(r.len(), 1);
        assert_eq!(r.saved_bytes(), 100_000_000);
    }

    #[test]
    fn release_reaches_zero_then_sweeps() {
        let mut r = LayerRegistry::new();
        let s = spec("alpine:3.20", 5_000_000);
        r.register(s.clone());
        r.register(s.clone());
        // One release: still referenced.
        assert_eq!(r.release(&s.digest), Some(false));
        assert!(r.get(&s.digest).is_some());
        // Second release: unreferenced, sweepable.
        assert_eq!(r.release(&s.digest), Some(true));
        let dead = r.sweep();
        assert_eq!(dead, vec![s.digest]);
        assert!(r.is_empty());
        // Unknown digest: None (not Some(false)).
        assert_eq!(r.release(&[9u8; 32]), None);
    }

    #[test]
    fn sharing_ratio_reflects_logical_vs_materialized() {
        let mut r = LayerRegistry::new();
        let s = spec("base:1", 1_000);
        r.register(s.clone());
        r.register(s.clone());
        r.register(s.clone());
        r.register(s.clone());
        // Logical 4000 / materialized 1000 = 4x sharing = 40_000 bps.
        assert_eq!(r.sharing_bps(), 40_000);
        assert_eq!(r.materialized_bytes(), 1_000);
        assert_eq!(r.logical_bytes(), 4_000);
    }

    #[test]
    fn empty_registry_is_neutral() {
        let r = LayerRegistry::new();
        assert_eq!(r.sharing_bps(), 10_000);
        assert_eq!(r.saved_bytes(), 0);
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn explicit_digest_override() {
        let mut r = LayerRegistry::new();
        let d = [7u8; 32];
        let s = spec("whatever", 10).with_digest(d);
        let (_, new) = r.register(s.clone());
        assert!(new);
        assert!(r.get(&d).is_some());
    }
}
