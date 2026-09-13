//! String helpers for formatting sizes/durations in CLI tool output
//! (`tools/*`), and validating file names against the constraints the
//! on-disk directory format actually has.

/// Formats a byte count the way `ls -h`/`df -h` would (KiB/MiB/GiB/TiB,
/// binary/1024-based units, one decimal place).
pub fn format_bytes_human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit_idx = 0;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit_idx])
    }
}

/// Whether `name` is usable as a single path component: nonempty, at most
/// `MAX_NAME_LEN` bytes, and containing neither NUL nor `/` (both of which
/// are structurally impossible to store in a POSIX directory entry / path).
pub fn is_valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= crate::common::constants::MAX_NAME_LEN
        && !name.contains('\0')
        && !name.contains('/')
        && name != "."
        && name != ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_common_sizes() {
        assert_eq!(format_bytes_human(0), "0 B");
        assert_eq!(format_bytes_human(1023), "1023 B");
        assert_eq!(format_bytes_human(1024), "1.0 KiB");
        assert_eq!(format_bytes_human(1024 * 1024 * 5), "5.0 MiB");
    }

    #[test]
    fn rejects_reserved_and_malformed_names() {
        assert!(!is_valid_file_name(""));
        assert!(!is_valid_file_name("."));
        assert!(!is_valid_file_name(".."));
        assert!(!is_valid_file_name("a/b"));
        assert!(!is_valid_file_name("a\0b"));
        assert!(!is_valid_file_name(&"x".repeat(300)));
    }

    #[test]
    fn accepts_ordinary_names() {
        assert!(is_valid_file_name("document.txt"));
        assert!(is_valid_file_name(".hidden"));
        assert!(is_valid_file_name("a"));
    }
}
