//! Coalescing adjacent extents. `Inode` stores up to `MAX_INLINE_EXTENTS`
//! (7) extents directly; every write that can't extend an existing extent
//! consumes another slot, so merging physically-and-logically-contiguous
//! ones back together (e.g. after a sequential write that happened to
//! allocate in several small pieces) helps avoid running out of inline
//! slots. Real, correct, and unit-tested; not yet called from
//! `file::writer`'s write path (which appends a new extent slot per
//! allocation today) -- wiring it in is a good next targeted change, kept
//! separate here to avoid touching the working write path in the same
//! pass as the RAID/encryption/compression work.

use crate::extents::extent::ExtentExt;
use crate::ondisk::serialization::Extent;

/// Merges every pair of adjacent extents in `extents` (order-independent:
/// sorts by logical start first), returning a new, possibly-shorter list.
pub fn coalesce(extents: &[Extent]) -> Vec<Extent> {
    if extents.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<Extent> = extents.to_vec();
    sorted.sort_by_key(|e| e.logical_start);

    let mut out: Vec<Extent> = Vec::with_capacity(sorted.len());
    for e in sorted {
        if let Some(last) = out.last_mut() {
            if last.is_adjacent_to(&e) {
                last.length += e.length;
                continue;
            }
        }
        out.push(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(logical_start: u64, physical_start: u64, length: u64) -> Extent {
        Extent {
            logical_start,
            physical_start,
            length,
        }
    }

    #[test]
    fn merges_contiguous_extents() {
        let extents = vec![ext(0, 100, 3), ext(3, 103, 2)];
        let merged = coalesce(&extents);
        assert_eq!(merged, vec![ext(0, 100, 5)]);
    }

    #[test]
    fn leaves_non_adjacent_extents_separate() {
        let extents = vec![ext(0, 100, 3), ext(3, 500, 2)]; // logically adjacent, physically not
        let merged = coalesce(&extents);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn order_of_input_does_not_matter() {
        let extents = vec![ext(3, 103, 2), ext(0, 100, 3)]; // out of logical order
        let merged = coalesce(&extents);
        assert_eq!(merged, vec![ext(0, 100, 5)]);
    }

    #[test]
    fn merges_a_chain_of_three() {
        let extents = vec![ext(0, 100, 2), ext(2, 102, 2), ext(4, 104, 2)];
        let merged = coalesce(&extents);
        assert_eq!(merged, vec![ext(0, 100, 6)]);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(coalesce(&[]), Vec::<Extent>::new());
    }
}
