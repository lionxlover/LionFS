//! A small iterator adapter over directory entries, for callers (readdir
//! implementations, `tools::dump`) that want to filter/map entries without
//! writing the same "skip `.`/`..`" or "only regular files" boilerplate
//! inline every time.

use crate::directory::entries::DirEntry;

pub struct DirEntryIter {
    inner: std::vec::IntoIter<DirEntry>,
}

impl DirEntryIter {
    pub fn new(entries: Vec<DirEntry>) -> Self {
        Self {
            inner: entries.into_iter(),
        }
    }

    /// Entries excluding the synthetic `.`/`..` entries a directory's own
    /// listing conventionally starts with.
    pub fn skip_dot_entries(self) -> impl Iterator<Item = DirEntry> {
        self.inner.filter(|e| e.name != "." && e.name != "..")
    }
}

impl Iterator for DirEntryIter {
    type Item = DirEntry;
    fn next(&mut self) -> Option<DirEntry> {
        self.inner.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, ino: u64, file_type: u8) -> DirEntry {
        DirEntry {
            ino,
            name: name.to_string(),
            file_type,
        }
    }

    #[test]
    fn skips_dot_and_dotdot() {
        let entries = vec![entry(".", 5, 2), entry("..", 1, 2), entry("real.txt", 9, 1)];
        let filtered: Vec<_> = DirEntryIter::new(entries).skip_dot_entries().collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "real.txt");
    }

    #[test]
    fn plain_iteration_preserves_order() {
        let entries = vec![entry("a", 2, 1), entry("b", 3, 1)];
        let names: Vec<_> = DirEntryIter::new(entries).map(|e| e.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
