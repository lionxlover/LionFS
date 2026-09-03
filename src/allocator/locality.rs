//! Locality grouping: preferring to allocate a file's new blocks near its
//! existing ones (or near its parent directory's blocks, for a brand new
//! file), which is what keeps sequential reads of a file sequential on
//! spinning media and improves readahead effectiveness generally. Not
//! currently consulted by `allocator::bitmap::Allocator::allocate_extents`
//! (which does a plain first-fit scan from the start of the block group);
//! wiring a locality *preference* into that scan is a real, valuable, but
//! separate change from what's been done to the allocator in this pass.

/// A preferred starting point to search from, given where a file's last
/// block was allocated (if any).
pub fn preferred_search_start(
    last_allocated_physical_block: Option<u64>,
    blocks_per_group: u32,
) -> u64 {
    match last_allocated_physical_block {
        Some(last) => last + 1,
        None => 0,
    }
    .min(blocks_per_group as u64)
}

/// Whether `candidate` is "close enough" to `reference` to count as
/// preserving locality, within `max_distance` blocks -- a threshold a
/// locality-aware search could use to prefer a nearby-but-not-immediately-
/// adjacent free run over scanning the whole group for a perfect match.
pub fn is_local(candidate: u64, reference: u64, max_distance: u64) -> bool {
    candidate.abs_diff(reference) <= max_distance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_right_after_the_last_block() {
        assert_eq!(preferred_search_start(Some(99), 1000), 100);
    }

    #[test]
    fn starts_at_zero_with_no_prior_allocation() {
        assert_eq!(preferred_search_start(None, 1000), 0);
    }

    #[test]
    fn clamps_to_group_size() {
        assert_eq!(preferred_search_start(Some(999), 1000), 1000);
    }

    #[test]
    fn locality_distance_check() {
        assert!(is_local(105, 100, 10));
        assert!(!is_local(200, 100, 10));
    }
}
