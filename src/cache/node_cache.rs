use crate::btree::node::BTreeNodeData;
use moka::sync::Cache;
use std::sync::{Arc, RwLock};

pub struct NodeCache {
    cache: Cache<u64, Arc<RwLock<BTreeNodeData>>>,
}

impl NodeCache {
    pub fn new(capacity: u64) -> Self {
        let cache = Cache::builder().max_capacity(capacity).build();
        Self { cache }
    }

    pub fn get(&self, block: u64) -> Option<Arc<RwLock<BTreeNodeData>>> {
        self.cache.get(&block)
    }

    pub fn insert(&self, block: u64, node: BTreeNodeData) {
        self.cache.insert(block, Arc::new(RwLock::new(node)));
    }

    pub fn invalidate(&self, block: u64) {
        self.cache.invalidate(&block);
    }
}
