use moka::sync::Cache;

#[derive(Clone)]
pub struct CachedDirEntry {
    pub ino: u64,
    pub file_type: u8,
}

pub struct DirCache {
    // Key is (Parent Inode, Child Name)
    cache: Cache<(u64, String), CachedDirEntry>,
}

impl DirCache {
    pub fn new(capacity: u64) -> Self {
        Self {
            cache: Cache::builder().max_capacity(capacity).build(),
        }
    }

    pub fn get(&self, parent_ino: u64, name: &str) -> Option<CachedDirEntry> {
        self.cache.get(&(parent_ino, name.to_string()))
    }

    pub fn insert(&self, parent_ino: u64, name: &str, ino: u64, file_type: u8) {
        self.cache.insert(
            (parent_ino, name.to_string()),
            CachedDirEntry { ino, file_type },
        );
    }

    pub fn remove(&self, parent_ino: u64, name: &str) {
        self.cache.invalidate(&(parent_ino, name.to_string()));
    }
}
