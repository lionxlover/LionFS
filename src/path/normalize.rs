//! Path normalization: collapsing `.`, `..`, and repeated `/` the way a
//! POSIX path resolver needs to before walking components against
//! directory entries. LionFS's FUSE layer gets paths pre-split by the
//! kernel (one `lookup()` call per component) so this isn't on the hot
//! path today, but any tool that accepts a user-supplied path string
//! directly (`tools/*` CLI utilities, `path::resolver`) needs it.

/// Normalizes an absolute or relative POSIX-style path into a canonical
/// component list: no empty components, `.` removed, `..` resolved against
/// preceding components where possible (a leading `..` on a relative path,
/// or one that would escape an absolute path's root, is kept as-is since
/// there's nothing to cancel it against).
pub fn normalize_components(path: &str) -> Vec<String> {
    let is_absolute = path.starts_with('/');
    let mut out: Vec<String> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                match out.last() {
                    Some(last) if last != ".." => {
                        out.pop();
                    }
                    _ => {
                        if !is_absolute {
                            out.push("..".to_string());
                        }
                        // At an absolute root, ".." has nowhere to go; drop it.
                    }
                }
            }
            _ => out.push(part.to_string()),
        }
    }
    out
}

/// Re-joins normalized components back into a path string.
pub fn normalize(path: &str) -> String {
    let is_absolute = path.starts_with('/');
    let components = normalize_components(path);
    let joined = components.join("/");
    if is_absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_repeated_slashes_and_dot() {
        assert_eq!(normalize("/a//b/./c"), "/a/b/c");
    }

    #[test]
    fn resolves_parent_references() {
        assert_eq!(normalize("/a/b/../c"), "/a/c");
        assert_eq!(normalize("/a/b/c/../../d"), "/a/d");
    }

    #[test]
    fn cannot_escape_absolute_root() {
        assert_eq!(normalize("/../../etc/passwd"), "/etc/passwd");
    }

    #[test]
    fn relative_leading_parent_is_preserved() {
        assert_eq!(normalize("../a/b"), "../a/b");
        assert_eq!(normalize("a/../../b"), "../b");
    }

    #[test]
    fn empty_and_root_cases() {
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("."), ".");
    }
}
