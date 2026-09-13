//! Splitting a single extent into pieces -- the inverse of
//! `extents::merge::coalesce`. Needed for operations that punch a hole in
//! the middle of a previously-contiguous run of blocks (e.g. a future
//! `FALLOC_FL_PUNCH_HOLE` implementation, or truncating a file down to a
//! size that lands in the middle of an extent) without disturbing the
//! parts of the extent on either side.

use crate::extents::extent::ExtentExt;
use crate::ondisk::serialization::Extent;

/// Splits `extent` at `logical_block` (which must fall strictly inside
/// it): returns `(before, after)`, each covering their respective side.
/// Returns `None` if `logical_block` isn't strictly inside `extent`
/// (splitting at an edge is a no-op, not an error, so callers can just
/// skip acting on `None` rather than handle a spurious error).
pub fn split_at(extent: &Extent, logical_block: u64) -> Option<(Extent, Extent)> {
    if logical_block <= extent.logical_start || logical_block >= extent.logical_end() {
        return None;
    }
    let offset = logical_block - extent.logical_start;
    let before = Extent {
        logical_start: extent.logical_start,
        physical_start: extent.physical_start,
        length: offset,
    };
    let after = Extent {
        logical_start: logical_block,
        physical_start: extent.physical_start + offset,
        length: extent.length - offset,
    };
    Some((before, after))
}

/// Removes the logical range `[start, end)` from `extent`, which may
/// leave zero, one, or two remaining pieces (zero if the removed range
/// covers the whole extent; two if it's a hole punched in the middle).
pub fn remove_range(extent: &Extent, start: u64, end: u64) -> Vec<Extent> {
    let mut out = Vec::new();
    if start > extent.logical_start {
        out.push(Extent {
            logical_start: extent.logical_start,
            physical_start: extent.physical_start,
            length: (start - extent.logical_start).min(extent.length),
        });
    }
    if end < extent.logical_end() {
        let clipped_end = end.max(extent.logical_start);
        let offset = clipped_end - extent.logical_start;
        out.push(Extent {
            logical_start: clipped_end,
            physical_start: extent.physical_start + offset,
            length: extent.length - offset,
        });
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
    fn split_in_the_middle() {
        let e = ext(0, 100, 10); // logical 0..10, physical 100..110
        let (before, after) = split_at(&e, 4).unwrap();
        assert_eq!(before, ext(0, 100, 4));
        assert_eq!(after, ext(4, 104, 6));
    }

    #[test]
    fn split_at_edge_returns_none() {
        let e = ext(0, 100, 10);
        assert!(split_at(&e, 0).is_none());
        assert!(split_at(&e, 10).is_none());
    }

    #[test]
    fn remove_range_covering_whole_extent_leaves_nothing() {
        let e = ext(5, 100, 5); // 5..10
        assert_eq!(remove_range(&e, 0, 100), Vec::new());
    }

    #[test]
    fn remove_range_from_the_middle_leaves_two_pieces() {
        let e = ext(0, 100, 10); // 0..10
        let pieces = remove_range(&e, 3, 6);
        assert_eq!(pieces, vec![ext(0, 100, 3), ext(6, 106, 4)]);
    }

    #[test]
    fn remove_range_from_the_start_leaves_the_tail() {
        let e = ext(0, 100, 10);
        let pieces = remove_range(&e, 0, 4);
        assert_eq!(pieces, vec![ext(4, 104, 6)]);
    }
}
