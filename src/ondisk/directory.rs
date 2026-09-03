//! Validation for `DirEntryHeader` records (defined in
//! `ondisk::serialization`) -- the fixed-header-plus-name-bytes format
//! `directory::entries::DirManager` reads/writes. Used by `tools::fsck` to
//! sanity-check a directory block without going through the full
//! `DirManager` read path (which assumes well-formed input).

use crate::ondisk::serialization::DirEntryHeader;

pub const MIN_RECORD_LEN: u16 = std::mem::size_of::<DirEntryHeader>() as u16;

/// Whether a single directory entry record's header is internally
/// consistent: its declared `rec_len` must be large enough to hold the
/// fixed header plus `name_len` bytes of name, and must fit within
/// whatever's left in the containing block.
pub fn validate_header(header: &DirEntryHeader, remaining_in_block: u16) -> Result<(), String> {
    let min_needed = MIN_RECORD_LEN + header.name_len as u16;
    if header.rec_len < min_needed {
        return Err(format!(
            "rec_len ({}) too small for name_len ({}): needs at least {min_needed}",
            header.rec_len, header.name_len
        ));
    }
    if header.rec_len > remaining_in_block {
        return Err(format!(
            "rec_len ({}) exceeds remaining space in block ({remaining_in_block})",
            header.rec_len
        ));
    }
    if header.file_type > 7 {
        return Err(format!(
            "file_type ({}) is outside the valid 0..=7 range",
            header.file_type
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(rec_len: u16, name_len: u8, file_type: u8) -> DirEntryHeader {
        DirEntryHeader {
            ino: 5,
            rec_len,
            name_len,
            file_type,
            padding: 0,
        }
    }

    #[test]
    fn valid_header_passes() {
        let h = header(MIN_RECORD_LEN + 8, 8, 1);
        assert!(validate_header(&h, 4096).is_ok());
    }

    #[test]
    fn rec_len_too_small_for_name_is_rejected() {
        let h = header(MIN_RECORD_LEN, 8, 1); // no room for 8 name bytes
        assert!(validate_header(&h, 4096).is_err());
    }

    #[test]
    fn rec_len_overflowing_block_is_rejected() {
        let h = header(MIN_RECORD_LEN + 8, 8, 1);
        assert!(validate_header(&h, 4).is_err());
    }

    #[test]
    fn invalid_file_type_is_rejected() {
        let h = header(MIN_RECORD_LEN + 8, 8, 200);
        assert!(validate_header(&h, 4096).is_err());
    }
}
