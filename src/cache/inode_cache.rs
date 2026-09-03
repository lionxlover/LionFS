use crate::ondisk::serialization::Inode;
use moka::sync::Cache;

#[derive(Clone)]
pub struct CachedInode {
    pub inode: Inode,
    pub dirty: bool,
}

pub struct InodeCache {
    cache: Cache<u64, CachedInode>,
}

impl InodeCache {
    pub fn new(capacity: u64) -> Self {
        Self {
            cache: Cache::builder().max_capacity(capacity).build(),
        }
    }

    pub fn get(&self, ino: u64) -> Option<Inode> {
        self.cache.get(&ino).map(|ci| ci.inode)
    }

    pub fn insert(&self, ino: u64, inode: Inode, dirty: bool) {
        self.cache.insert(ino, CachedInode { inode, dirty });
    }

    pub fn mark_dirty(&self, ino: u64) {
        if let Some(mut ci) = self.cache.get(&ino) {
            ci.dirty = true;
            self.cache.insert(ino, ci);
        }
    }

    pub fn remove(&self, ino: u64) {
        self.cache.invalidate(&ino);
    }
}
