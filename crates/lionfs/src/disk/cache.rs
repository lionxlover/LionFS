//! A raw block read cache, keyed by (device index, physical block) --
//! below the node/inode caches in `cache::*` (which cache *parsed*
//! structures), this caches raw bytes as read from a device. Most useful
//! for RAID5/6, where a single logical write reads several other physical
//! blocks in the same stripe row to recompute parity (`Disk::write_block_parity`);
//! repeated writes to the same row benefit from not re-reading unchanged
//! siblings from disk every time. Not currently wired into `Disk` itself
//! (that integration -- deciding when entries get invalidated by writes --
//! deserves its own careful pass rather than being bundled into the
//! RAID/encryption work already done in this one); this is the standalone,
//! working cache it would sit on top of.

use moka::sync::Cache;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub device: usize,
    pub physical_block: u64,
}

pub struct RawBlockCache {
    cache: Cache<BlockKey, std::sync::Arc<Vec<u8>>>,
}

impl RawBlockCache {
    pub fn new(capacity: u64) -> Self {
        Self {
            cache: Cache::builder().max_capacity(capacity).build(),
        }
    }

    pub fn get(&self, device: usize, physical_block: u64) -> Option<std::sync::Arc<Vec<u8>>> {
        self.cache.get(&BlockKey {
            device,
            physical_block,
        })
    }

    pub fn insert(&self, device: usize, physical_block: u64, data: Vec<u8>) {
        self.cache.insert(
            BlockKey {
                device,
                physical_block,
            },
            std::sync::Arc::new(data),
        );
    }

    /// Must be called whenever a block is written, or reads would keep
    /// returning stale cached content.
    pub fn invalidate(&self, device: usize, physical_block: u64) {
        self.cache.invalidate(&BlockKey {
            device,
            physical_block,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_and_returns_block_data() {
        let cache = RawBlockCache::new(100);
        assert!(cache.get(0, 5).is_none());
        cache.insert(0, 5, vec![1, 2, 3]);
        assert_eq!(*cache.get(0, 5).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn different_devices_with_same_block_number_are_distinct_entries() {
        let cache = RawBlockCache::new(100);
        cache.insert(0, 5, vec![1]);
        cache.insert(1, 5, vec![2]);
        assert_eq!(*cache.get(0, 5).unwrap(), vec![1]);
        assert_eq!(*cache.get(1, 5).unwrap(), vec![2]);
    }

    #[test]
    fn invalidate_removes_the_entry() {
        let cache = RawBlockCache::new(100);
        cache.insert(0, 5, vec![1, 2, 3]);
        cache.invalidate(0, 5);
        assert!(cache.get(0, 5).is_none());
    }
}
