//! Pluggable allocation strategies over a list of candidate free extents.
//! `allocator::bitmap::Allocator` currently always does first-fit
//! internally (scan the bitmap, take the first run big enough); this
//! module expresses that choice -- and alternatives -- as an explicit,
//! swappable policy over `(start, length)` candidates, for callers that
//! already have a candidate list (e.g. from `FreeSpaceTree`) and want to
//! choose among them, rather than a from-scratch bitmap scan.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationPolicy {
    /// First candidate that's big enough. Fast; matches what the bitmap
    /// scanner already does.
    FirstFit,
    /// Smallest candidate that's still big enough -- minimizes leftover
    /// fragmentation from this specific allocation, at the cost of a full
    /// scan of the candidate list.
    BestFit,
    /// Largest available candidate, preferring to leave many small free
    /// extents alone rather than carve up the biggest one.
    WorstFit,
}

/// Picks a candidate satisfying `needed_blocks` from `candidates`
/// (`(start, length)` pairs), or `None` if nothing is big enough.
pub fn choose(
    candidates: &[(u64, u64)],
    needed_blocks: u64,
    policy: AllocationPolicy,
) -> Option<(u64, u64)> {
    let fits = candidates
        .iter()
        .copied()
        .filter(|(_, len)| *len >= needed_blocks);
    match policy {
        AllocationPolicy::FirstFit => fits.into_iter().next(),
        AllocationPolicy::BestFit => fits.min_by_key(|(_, len)| *len),
        AllocationPolicy::WorstFit => fits.max_by_key(|(_, len)| *len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANDIDATES: &[(u64, u64)] = &[(0, 5), (100, 20), (200, 8), (300, 50)];

    #[test]
    fn first_fit_takes_the_first_big_enough() {
        assert_eq!(
            choose(CANDIDATES, 8, AllocationPolicy::FirstFit),
            Some((100, 20))
        );
    }

    #[test]
    fn best_fit_takes_the_smallest_sufficient() {
        assert_eq!(
            choose(CANDIDATES, 8, AllocationPolicy::BestFit),
            Some((200, 8))
        );
    }

    #[test]
    fn worst_fit_takes_the_largest() {
        assert_eq!(
            choose(CANDIDATES, 8, AllocationPolicy::WorstFit),
            Some((300, 50))
        );
    }

    #[test]
    fn returns_none_when_nothing_fits() {
        assert_eq!(choose(CANDIDATES, 1000, AllocationPolicy::FirstFit), None);
    }
}
