//! A structured error type for LionFS-specific failure modes.
//!
//! The existing codebase plumbs `std::io::Error` everywhere, which works
//! fine for I/O failures but loses information for filesystem-logic errors
//! (a checksum mismatch and a "disk full" condition currently look
//! identical to a caller). `LfsError` gives new code a way to be specific,
//! while `From<LfsError> for io::Error` means it drops into any existing
//! `-> std::io::Result<T>` function via `?` without forcing a signature
//! change -- this is additive, not a rewrite of the existing error paths.
use std::fmt;
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LfsError {
    ChecksumMismatch { object_id: u64, logical_block: u64 },
    CorruptMetadata(String),
    NoSpaceLeft,
    InodeNotFound(u64),
    NotADirectory(u64),
    IsADirectory(u64),
    InvalidName(String),
    PermissionDenied,
    UnsupportedAlgorithm { kind: &'static str, id: u8 },
    EncryptionKeyMissing(u32),
    RaidDegradedBeyondRecovery { profile: &'static str },
    InvalidSuperblock(String),
}

impl fmt::Display for LfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LfsError::ChecksumMismatch {
                object_id,
                logical_block,
            } => {
                write!(
                    f,
                    "checksum mismatch for inode {object_id}, logical block {logical_block}"
                )
            }
            LfsError::CorruptMetadata(msg) => write!(f, "corrupt on-disk metadata: {msg}"),
            LfsError::NoSpaceLeft => write!(f, "no space left on device"),
            LfsError::InodeNotFound(ino) => write!(f, "inode {ino} not found"),
            LfsError::NotADirectory(ino) => write!(f, "inode {ino} is not a directory"),
            LfsError::IsADirectory(ino) => write!(f, "inode {ino} is a directory"),
            LfsError::InvalidName(name) => write!(f, "invalid file name: {name:?}"),
            LfsError::PermissionDenied => write!(f, "permission denied"),
            LfsError::UnsupportedAlgorithm { kind, id } => {
                write!(f, "unsupported {kind} algorithm id {id}")
            }
            LfsError::EncryptionKeyMissing(key_id) => {
                write!(f, "encryption key {key_id} not found")
            }
            LfsError::RaidDegradedBeyondRecovery { profile } => write!(
                f,
                "{profile} array has failed beyond what its redundancy can recover"
            ),
            LfsError::InvalidSuperblock(msg) => write!(f, "invalid superblock: {msg}"),
        }
    }
}

impl std::error::Error for LfsError {}

impl From<LfsError> for io::Error {
    fn from(e: LfsError) -> io::Error {
        let kind = match &e {
            LfsError::ChecksumMismatch { .. }
            | LfsError::CorruptMetadata(_)
            | LfsError::InvalidSuperblock(_) => io::ErrorKind::InvalidData,
            LfsError::NoSpaceLeft => io::ErrorKind::Other,
            LfsError::InodeNotFound(_) => io::ErrorKind::NotFound,
            LfsError::NotADirectory(_) => io::ErrorKind::Other,
            LfsError::IsADirectory(_) => io::ErrorKind::Other,
            LfsError::InvalidName(_) => io::ErrorKind::InvalidInput,
            LfsError::PermissionDenied => io::ErrorKind::PermissionDenied,
            LfsError::UnsupportedAlgorithm { .. } => io::ErrorKind::InvalidInput,
            LfsError::EncryptionKeyMissing(_) => io::ErrorKind::InvalidInput,
            LfsError::RaidDegradedBeyondRecovery { .. } => io::ErrorKind::Other,
        };
        io::Error::new(kind, e.to_string())
    }
}

pub type LfsResult<T> = Result<T, LfsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_into_io_error_with_matching_kind() {
        let e: io::Error = LfsError::InodeNotFound(42).into();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("42"));
    }

    #[test]
    fn question_mark_operator_composes_with_io_result() {
        fn inner() -> LfsResult<u64> {
            Err(LfsError::NoSpaceLeft)
        }
        fn outer() -> io::Result<u64> {
            Ok(inner()?)
        }
        assert!(outer().is_err());
    }
}
