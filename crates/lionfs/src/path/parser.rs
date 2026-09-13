//! Splitting a path string into components and extracting basename/dirname
//! -- the string-level counterpart to `path::normalize`, for tools that
//! work with LionFS path strings directly rather than `std::path::Path`.

/// Splits a (normalized or not) path into non-empty components.
pub fn split_components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

/// The final component of a path (`"a/b/c" -> "c"`), or `None` for a path
/// with no components (empty string or root `/`).
pub fn basename(path: &str) -> Option<&str> {
    split_components(path).last().copied()
}

/// Everything before the final component, re-joined with a leading `/` if
/// the input was absolute (`"/a/b/c" -> "/a/b"`, `"a/b/c" -> "a/b"`).
/// Returns `"/"` for a root or single-component absolute path, `""` for a
/// single-component relative path.
pub fn dirname(path: &str) -> String {
    let is_absolute = path.starts_with('/');
    let mut parts = split_components(path);
    parts.pop();
    let joined = parts.join("/");
    if is_absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_components() {
        assert_eq!(split_components("/a/b/c"), vec!["a", "b", "c"]);
        assert_eq!(split_components("a//b/"), vec!["a", "b"]);
    }

    #[test]
    fn basename_cases() {
        assert_eq!(basename("/a/b/c.txt"), Some("c.txt"));
        assert_eq!(basename("/"), None);
        assert_eq!(basename(""), None);
    }

    #[test]
    fn dirname_cases() {
        assert_eq!(dirname("/a/b/c.txt"), "/a/b");
        assert_eq!(dirname("/c.txt"), "/");
        assert_eq!(dirname("a/b"), "a");
        assert_eq!(dirname("a"), "");
    }
}
