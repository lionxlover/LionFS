//! # Container & VM Awareness (RFC-004 §10)
//!
//! Two features that make LionFS a native substrate for
//! container-host and VM-host workloads rather than a bystander:
//!
//! * [`layers`] -- image-layer content-addressable storage: container
//!   image layers are *the* dedup jackpot (a 50-container host runs
//!   one copy of the base layer). Layers register by content digest;
//!   files within a layer map digest -> extent set with refcounts, so
//!   two containers' identical `/usr/lib/x/y.so` share physical
//!   extents and the refcount subsystem keeps them alive as long as
//!   any layer references them. This composes with the existing
//!   FastCDC/three-level dedup index (RFC-002 §8.4) rather than
//!   replacing it: layer registration is the *policy* hint that says
//!   "this subtree is a clone source; pin its chunks in the hot
//!   dedup index."
//! * [`virtiofs`] -- the passthrough policy table: which host paths a
//!   virtiofs/virtio-blk tag exports, with cache-model and DAX hints,
//!   so the mount layer can serve VM guests from LionFS volumes
//!   without a double page cache.
//!
//! Both are policy layers over the 3.0 substrate; the data plane
//! (extent sharing, refcounts, checksums) is unchanged.

pub mod layers;
pub mod virtiofs;

pub use layers::{LayerDigest, LayerRegistry, LayerSpec};
pub use virtiofs::{CacheModel, PassthroughTable, PassthroughTarget};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_compose() {
        let mut r = LayerRegistry::new();
        let _ = r.register(LayerSpec::new("sha256:abc", 1024, 8));
        let mut t = PassthroughTable::new();
        let _ = t.add("/var/lib/lionfs/vm/root", "vm0-root", CacheModel::Always);
    }
}
