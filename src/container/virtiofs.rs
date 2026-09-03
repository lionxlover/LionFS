//! Virtiofs passthrough policy (RFC-004 §10.2).
//!
//! The table maps host paths to virtiofs export tags with cache-model
//! and DAX hints, so the mount layer serves VM guests directly from
//! LionFS volumes: one page cache on the host, no double caching in
//! the guest beyond what the cache model permits, and LionFS
//! checksumming/scrubbing still covers every byte the guest touches
//! (the guest talks to a socket, not to the device).
//!
//! Cache models are the virtiofs spec's own; the table stores the
//! operator's intent and the bridge enforces it:
//!
//! * `None` -- guest caches nothing: strongest consistency, for
//!   shared writable mounts (live migration targets, shared scratch).
//! * `Always` -- guest caches aggressively: for private per-VM root
//!   disks; the host invalidates on foreign writes.
//! * `Auto` -- guest caches with revalidation per open: the default.

use std::collections::BTreeMap;

/// Virtiofs cache model (spec terms).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheModel {
    None,
    Auto,
    Always,
}

impl CacheModel {
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Auto => "auto",
            Self::Always => "always",
        }
    }
}

/// One passthrough export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassthroughTarget {
    /// Host-side path (must live on a LionFS volume at mount time).
    pub host_path: String,
    /// The virtiofs tag the guest mounts (`-device
    /// virtio-fs-pci,tag=...`).
    pub tag: String,
    /// Cache model intent.
    pub cache_model: CacheModel,
    /// DAX: map host pages into the guest directly (no bounce);
    /// requires shared-memory-capable backend (CXL tier preferred).
    pub dax: bool,
    /// Squash guest ownership to the export's uid/gid (the
    /// "no root in my host" posture).
    pub squash_ids: bool,
}

/// The policy table, ordered by host path for deterministic dumps.
pub struct PassthroughTable {
    exports: BTreeMap<String, PassthroughTarget>,
}

impl PassthroughTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            exports: BTreeMap::new(),
        }
    }

    /// Adds an export. Tag collision (two host paths exporting the
    /// same tag) is refused: the guest could not tell them apart.
    pub fn add(
        &mut self,
        host_path: &str,
        tag: &str,
        cache_model: CacheModel,
    ) -> Result<(), &'static str> {
        let clash = self
            .exports
            .values()
            .any(|e| e.tag == tag && e.host_path != host_path);
        if clash {
            return Err("tag already exported from another host path");
        }
        let target = PassthroughTarget {
            host_path: host_path.to_owned(),
            tag: tag.to_owned(),
            cache_model,
            dax: false,
            squash_ids: false,
        };
        self.exports.insert(host_path.to_owned(), target);
        Ok(())
    }

    /// Configures DAX + identity squashing on an existing export.
    pub fn tune(&mut self, host_path: &str, dax: bool, squash_ids: bool) -> Option<()> {
        let e = self.exports.get_mut(host_path)?;
        e.dax = dax;
        e.squash_ids = squash_ids;
        Some(())
    }

    /// Lookup by host path.
    #[must_use]
    pub fn get(&self, host_path: &str) -> Option<&PassthroughTarget> {
        self.exports.get(host_path)
    }

    /// Lookup by guest-visible tag.
    #[must_use]
    pub fn by_tag(&self, tag: &str) -> Option<&PassthroughTarget> {
        self.exports.values().find(|e| e.tag == tag)
    }

    /// All exports, ordered by host path.
    #[must_use]
    pub fn exports(&self) -> Vec<&PassthroughTarget> {
        self.exports.values().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.exports.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exports.is_empty()
    }
}

impl Default for PassthroughTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_lookup_both_ways() {
        let mut t = PassthroughTable::new();
        t.add("/var/lib/lionfs/vm/root", "vm0-root", CacheModel::Always)
            .expect("unique tag");
        assert_eq!(t.get("/var/lib/lionfs/vm/root").map(|e| e.tag.as_str()), Some("vm0-root"));
        assert_eq!(
            t.by_tag("vm0-root").map(|e| e.host_path.as_str()),
            Some("/var/lib/lionfs/vm/root")
        );
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn tag_collisions_across_paths_are_refused() {
        let mut t = PassthroughTable::new();
        t.add("/a", "shared", CacheModel::None).expect("first");
        let err = t.add("/b", "shared", CacheModel::None).expect_err("collision");
        assert_eq!(err, "tag already exported from another host path");
        // Same path re-adding with the same tag is a replace, not a clash.
        t.add("/a", "shared", CacheModel::Auto).expect("replace");
        assert_eq!(t.get("/a").map(|e| e.cache_model), Some(CacheModel::Auto));
    }

    #[test]
    fn tuning_flips_dax_and_squash() {
        let mut t = PassthroughTable::new();
        t.add("/cxl/pool0/vm", "cxl-vm", CacheModel::Auto).expect("add");
        assert!(!t.get("/cxl/pool0/vm").expect("known").dax);
        t.tune("/cxl/pool0/vm", true, true).expect("tune");
        let e = t.get("/cxl/pool0/vm").expect("known");
        assert!(e.dax);
        assert!(e.squash_ids);
        assert!(t.tune("/missing", true, true).is_none());
    }

    #[test]
    fn exports_dump_in_path_order() {
        let mut t = PassthroughTable::new();
        t.add("/z", "z", CacheModel::None).expect("add");
        t.add("/a", "a", CacheModel::None).expect("add");
        t.add("/m", "m", CacheModel::None).expect("add");
        let paths: Vec<&str> = t.exports().iter().map(|e| e.host_path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/m", "/z"]);
    }

    #[test]
    fn cache_model_tags() {
        assert_eq!(CacheModel::None.tag(), "none");
        assert_eq!(CacheModel::Auto.tag(), "auto");
        assert_eq!(CacheModel::Always.tag(), "always");
    }
}
