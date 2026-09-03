//! A facade composing `allocator::bitmap::Allocator` (the live bitmap
//! scanner) with the policy/locality/statistics modules in this
//! directory. Kept separate from `allocator::bitmap::Allocator` itself --
//! which is the struct `file::writer`/`fs::filesystem` actually call
//! through today -- rather than modifying that live, working call path as
//! part of this pass. This is a real, usable composition new code can
//! adopt; it is not currently on the hot path.

use crate::allocator::locality::preferred_search_start;
use crate::allocator::statistics::{analyze, FragmentationStats};
use crate::ondisk::serialization::BlockGroupDescriptor;
use crate::transaction::transaction::TxContext;
use std::io::Result;

pub struct BlockAllocator {
    pub last_allocated: Option<u64>,
}

impl Default for BlockAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockAllocator {
    pub fn new() -> Self {
        Self {
            last_allocated: None,
        }
    }

    /// Allocates `count` blocks via the real bitmap allocator, recording
    /// where it landed so the *next* call can bias its search toward
    /// locality (see `allocator::locality`) instead of always starting
    /// from the beginning of the group.
    pub fn allocate(
        &mut self,
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        count: u64,
    ) -> Result<u64> {
        let _preferred_start = preferred_search_start(self.last_allocated, blocks_per_group);
        // `Allocator::allocate_extents` doesn't currently take a search-start
        // hint (it always scans from the beginning of the group), so
        // `_preferred_start` isn't passed through yet -- that's the actual
        // wiring step still needed to make locality bias real; what's
        // tracked here is ready for it.
        let start = crate::allocator::bitmap::Allocator::allocate_extents(
            ctx,
            bg_desc,
            blocks_per_group,
            count,
        )?;
        self.last_allocated = Some(start + count - 1);
        Ok(start)
    }

    pub fn fragmentation(&self, free_extents: &[(u64, u64)]) -> FragmentationStats {
        analyze(free_extents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_last_allocated_block_for_locality() {
        let mut alloc = BlockAllocator::new();
        assert_eq!(alloc.last_allocated, None);
        alloc.last_allocated = Some(50); // simulate a prior allocation without needing a real TxContext
        assert_eq!(
            crate::allocator::locality::preferred_search_start(alloc.last_allocated, 1000),
            51
        );
    }
}
