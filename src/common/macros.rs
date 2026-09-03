//! A couple of small declarative macros used to cut down on boilerplate in
//! new code. Exported at the crate root via `#[macro_export]` so callers
//! can use them as `lionfs_core::lfs_bail!(...)` / `lionfs_core::lfs_ensure!(...)`
//! without an explicit `use` of this module.

/// Early-returns `Err(...)` (converted via `.into()`, so it works with
/// both `std::io::Error` and `common::errors::LfsError` call sites) with a
/// formatted message. Shorthand for
/// `return Err(std::io::Error::new(std::io::ErrorKind::Other, format!(...)))`.
#[macro_export]
macro_rules! lfs_bail {
    ($($arg:tt)*) => {
        return Err(::std::io::Error::new(::std::io::ErrorKind::Other, format!($($arg)*)).into())
    };
}

/// `assert!`-shaped early return: if `$cond` is false, bails with the
/// given message instead of panicking. Useful for validating on-disk data
/// (a malformed superblock/inode should be a recoverable error, not a
/// panic that takes the whole process down).
#[macro_export]
macro_rules! lfs_ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !($cond) {
            $crate::lfs_bail!($($arg)*);
        }
    };
}

#[cfg(test)]
mod tests {
    fn check(n: i32) -> std::io::Result<i32> {
        lfs_ensure!(n >= 0, "n must be non-negative, got {n}");
        Ok(n * 2)
    }

    #[test]
    fn ensure_passes_through_on_success() {
        assert_eq!(check(5).unwrap(), 10);
    }

    #[test]
    fn ensure_bails_with_message_on_failure() {
        let err = check(-1).unwrap_err();
        assert!(err.to_string().contains("non-negative"));
    }
}
