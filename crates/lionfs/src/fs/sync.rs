//! Whole-filesystem sync policy: when to force an early transaction commit
//! versus let it batch up to the existing 1024-dirty-block threshold
//! (`fs::filesystem::LionFS::write`). Pulled out as a named policy rather
//! than a magic number inline, so it can be tuned/reasoned about in one
//! place.

#[derive(Debug, Clone, Copy)]
pub struct SyncPolicy {
    pub max_dirty_blocks: usize,
}

impl Default for SyncPolicy {
    fn default() -> Self {
        Self {
            max_dirty_blocks: 1024,
        }
    }
}

impl SyncPolicy {
    pub fn should_commit_now(&self, dirty_block_count: usize) -> bool {
        dirty_block_count > self.max_dirty_blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_once_threshold_exceeded() {
        let policy = SyncPolicy {
            max_dirty_blocks: 10,
        };
        assert!(!policy.should_commit_now(10));
        assert!(policy.should_commit_now(11));
    }
}
