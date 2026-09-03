//! Small shared traits used to write generic helper code against LionFS's
//! on-disk types without duplicating per-type boilerplate.

use crate::integrity::algorithms::{calculate_checksum, verify_checksum, ChecksumAlgorithm};

/// Anything that can compute and verify its own checksum over a byte
/// representation. Implemented here for `&[u8]` directly (the common
/// case: checksumming a raw block buffer) rather than requiring every
/// on-disk struct to implement it individually.
pub trait Checksummable {
    fn checksum_with(&self, algo: ChecksumAlgorithm) -> [u8; 32];
    fn verify_checksum_with(&self, algo: ChecksumAlgorithm, expected: &[u8; 32]) -> bool;
}

impl Checksummable for [u8] {
    fn checksum_with(&self, algo: ChecksumAlgorithm) -> [u8; 32] {
        calculate_checksum(algo, self)
    }
    fn verify_checksum_with(&self, algo: ChecksumAlgorithm, expected: &[u8; 32]) -> bool {
        verify_checksum(algo, self, expected)
    }
}

/// A resource that batches changes and must be explicitly finalized --
/// implemented by `transaction::transaction::Transaction` conceptually
/// (commit-or-rollback); expressed here as a trait so generic code (e.g. a
/// future "run this closure in a transaction, roll back on error" helper)
/// can be written once against any type that provides these two
/// operations, rather than being hand-written per call site.
pub trait Finalizable {
    type Error;
    fn commit(self) -> Result<(), Self::Error>;
    fn rollback(self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_slice_checksums_via_trait() {
        let data = b"some data";
        let csum = data.as_slice().checksum_with(ChecksumAlgorithm::Sha256);
        assert!(data
            .as_slice()
            .verify_checksum_with(ChecksumAlgorithm::Sha256, &csum));
    }
}
