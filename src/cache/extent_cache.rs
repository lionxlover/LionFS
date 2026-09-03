use crate::ondisk::serialization::Extent;
use moka::sync::Cache;
use std::sync::Arc;

#[derive(Clone)]
pub struct CachedExtents {
    pub extents: Vec<Extent>,
    pub dirty: bool,
}

pub struct ExtentCache {
    // Key is Inode Number
    cache: Cache<u64, Arc<CachedExtents>>,
}

impl ExtentCache {
    pub fn new(capacity: u64) -> Self {
        Self {
            cache: Cache::builder().max_capacity(capacity).build(),
        }
    }

    pub fn get(&self, ino: u64) -> Option<Arc<CachedExtents>> {
        self.cache.get(&ino)
    }

    pub fn insert(&self, ino: u64, extents: Vec<Extent>, dirty: bool) {
        self.cache
            .insert(ino, Arc::new(CachedExtents { extents, dirty }));
    }

    pub fn mark_dirty(&self, ino: u64) {
        if let Some(ce) = self.cache.get(&ino) {
            let mut new_ce = (*ce).clone();
            new_ce.dirty = true;
            self.cache.insert(ino, Arc::new(new_ce));
        }
    }

    pub fn remove(&self, ino: u64) {
        self.cache.invalidate(&ino);
    }
}
