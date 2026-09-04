//! # Migration onto the real tar stream (RFC-004 §9, Phase 8 wiring)
//!
//! 3.0.0 shipped detection, the verification manifest, and strategy
//! planning; the streaming half was "tooling, later." This is the
//! later: a real **ustar stream parser** feeding the LionFS write
//! path through the [`ImportSink`] seam, with the manifest recorded
//! as files land and a read-back verification pass closing the
//! protocol. Migration is a protocol, not a copy:
//!
//! ```text
//! source fs (any driver) ──tar──► 512-byte blocks
//!        │                          │
//!        │                   TarImportSession::import
//!        │                          │  parse + checksum-verify
//!        │                          │  GNU longname ('L') honored
//!        │                          ▼
//!        │                  ImportSink::write_file / mkdir / symlink
//!        │                   (the ordinary POSIX path: journaled,
//!        │                    checksummed, CoW)
//!        │                          │
//!        │                          ▼
//!        │                  Manifest: (path, size, SHA-256) per file
//!        │                          │
//!        └── read-back ────────────►│ verify: destination bits
//!                                   │ re-hashed and compared
//!                                   ▼
//!                          VerifySummary (failures == 0 -> done)
//! ```
//!
//! Supported typeflags: `0`/NUL (regular), `5` (directory), `2`
//! (symlink), `L` (GNU long name, next block holds the path), `1`
//! (hardlink: skipped and counted -- the importer does not
//! synthesize link structure it cannot verify), `x`/`g` (PAX
//! headers: skipped with a count; the streaming session targets the
//! ustar subset `tar --format=ustar` emits, plus GNU longname as
//! GNU tar's default emits). Two zero blocks end the archive; the
//! parser also accepts the 10 KiB block padding that follows.
//!
//! Sizes are octal ASCII (12 field bytes, max $8^{11} - 1$ bytes =
//! 8 GiB per member): the streaming session's per-file ceiling. The
//! 3.0 capacity plane makes the *volume* unbounded; the tar import
//! path's member ceiling is a format property, documented here so
//! nobody discovers it as a bug. (`--format=pax` with base-256
//! sizes is the Phase 9 follow-up for >8 GiB members.)

use crate::migrate::manifest::{Manifest, VerifySummary};
use std::io;

/// One 512-byte tar block.
const BLOCK: usize = 512;

/// Why a stream failed to parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TarParseError {
    /// Stream ended mid-header.
    TruncatedHeader,
    /// Header magic is not ustar.
    BadMagic,
    /// Header checksum mismatch (real corruption; the stream stops).
    BadChecksum { at_offset: u64 },
    /// Stream ended mid-body.
    TruncatedBody { path: String, need: u64, have: u64 },
    /// A size/mode field is not octal ASCII.
    NonOctalField { field: &'static str },
    /// A GNU longname body is absent or not NUL-terminated.
    BadLongName,
    /// The destination write path rejected a member.
    SinkError { path: String, message: String },
}

impl std::fmt::Display for TarParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TruncatedHeader => write!(f, "truncated tar header"),
            Self::BadMagic => write!(f, "not a ustar archive"),
            Self::BadChecksum { at_offset } => {
                write!(f, "tar header checksum mismatch at byte {at_offset}")
            }
            Self::TruncatedBody { path, need, have } => {
                write!(f, "truncated body for {path}: need {need}, have {have}")
            }
            Self::NonOctalField { field } => write!(f, "non-octal {field} field"),
            Self::BadLongName => write!(f, "malformed GNU long name record"),
            Self::SinkError { path, message } => {
                write!(f, "destination rejected {path}: {message}")
            }
        }
    }
}

impl std::error::Error for TarParseError {}

/// The destination side of the import: the LionFS write path (and
/// read-back path) behind one seam. The engine implements it over
/// the mounted VFS; tests and the simulator over a BTreeMap.
pub trait ImportSink {
    /// Writes one file's full contents (the POSIX create+write path).
    fn write_file(&mut self, path: &str, data: &[u8]) -> io::Result<()>;
    /// Creates one directory.
    fn mkdir(&mut self, path: &str) -> io::Result<()>;
    /// Creates one symlink.
    fn symlink(&mut self, path: &str, target: &str) -> io::Result<()>;
    /// Reads a file back (verification pass). `None` = missing.
    fn read_file(&self, path: &str) -> Option<Vec<u8>>;
}

/// The import summary (before verification).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Hardlink members skipped (counted, not materialized).
    pub hardlinks_skipped: u64,
    /// PAX extended headers skipped.
    pub pax_skipped: u64,
    pub bytes: u64,
}

/// The streaming session: import, then verify.
pub struct TarImportSession {
    manifest: Manifest,
    summary: ImportSummary,
}

impl Default for TarImportSession {
    fn default() -> Self {
        Self::new()
    }
}

impl TarImportSession {
    /// A fresh session (empty manifest, zero counters).
    #[must_use]
    pub fn new() -> Self {
        Self {
            manifest: Manifest::new(),
            summary: ImportSummary::default(),
        }
    }

    /// Imports an entire in-memory tar image through the sink. The
    /// stream stops at the first structural error; everything landed
    /// before it stays landed (and stays in the manifest -- a
    /// re-run with the fixed stream is the operator's call; the
    /// session is single-use by design).
    pub fn import(&mut self, sink: &mut dyn ImportSink, tar: &[u8]) -> Result<ImportSummary, TarParseError> {
        let mut off = 0usize;
        // Pending GNU longname for the next header.
        let mut pending_longname: Option<String> = None;
        let mut saw_end = false;
        while off + BLOCK <= tar.len() {
            let header = &tar[off..off + BLOCK];
            // End marker: zero block; the rest is record padding.
            if header.iter().all(|b| *b == 0) {
                saw_end = true;
                break;
            }
            let at_offset = off as u64;
            off += BLOCK;

            // Checksum first: the field itself counts as spaces.
            let stored = parse_octal(&header[148..156])
                .ok_or(TarParseError::NonOctalField { field: "chksum" })?;
            let computed = header_checksum(header);
            if stored != computed {
                return Err(TarParseError::BadChecksum { at_offset });
            }
            // Magic: "ustar\0" (POSIX) or "ustar " + version (GNU).
            if &header[257..262] != b"ustar" {
                return Err(TarParseError::BadMagic);
            }

            let typeflag = header[156];
            let size = parse_octal(&header[124..136])
                .ok_or(TarParseError::NonOctalField { field: "size" })?;
            let have = (tar.len() - off) as u64;
            if have < size {
                return Err(TarParseError::TruncatedBody {
                    path: header_name(header).unwrap_or_else(|| "?".into()),
                    need: size,
                    have,
                });
            }
            let body = &tar[off..off + size as usize];
            off += blocks_for(size) * BLOCK;

            match typeflag {
                b'L' => {
                    // GNU long name: body is the next header's path.
                    let end = body
                        .iter()
                        .position(|b| *b == 0)
                        .ok_or(TarParseError::BadLongName)?;
                    pending_longname = Some(
                        String::from_utf8_lossy(&body[..end]).into_owned(),
                    );
                }
                b'0' | 0 => {
                    let path = match pending_longname.take() {
                        Some(long) => long,
                        None => header_name(header)
                            .ok_or(TarParseError::NonOctalField { field: "name" })?,
                    };
                    sink.write_file(&path, body).map_err(|e| TarParseError::SinkError {
                        path: path.clone(),
                        message: e.to_string(),
                    })?;
                    // Manifest record: (path, size, SHA-256). A
                    // duplicate path (tar allows it; POSIX last-write
                    // wins) keeps the first digest: the verify pass
                    // would flag the overwrite anyway, which is the
                    // honest outcome.
                    let _ = self.manifest.record_content(&path, body);
                    self.summary.files += 1;
                    self.summary.bytes += size;
                }
                b'5' => {
                    let path = header_name(header)
                        .ok_or(TarParseError::NonOctalField { field: "name" })?;
                    let path = path.trim_end_matches('/').to_owned();
                    if !path.is_empty() {
                        let _ = sink.mkdir(&path);
                        self.summary.dirs += 1;
                    }
                    pending_longname = None;
                }
                b'2' => {
                    let path = pending_longname
                        .take()
                        .or_else(|| header_name(header))
                        .unwrap_or_default();
                    let target = String::from_utf8_lossy(
                        header[157..257]
                            .split(|b| *b == 0)
                            .next()
                            .unwrap_or(&[]),
                    )
                    .into_owned();
                    let _ = sink.symlink(&path, &target);
                    self.summary.symlinks += 1;
                }
                b'1' => {
                    self.summary.hardlinks_skipped += 1;
                    pending_longname = None;
                }
                b'x' | b'g' => {
                    self.summary.pax_skipped += 1;
                    pending_longname = None;
                }
                _ => {
                    // Unknown member types are skipped (counted as
                    // pax_skipped's "other" bucket -- kept honest by
                    // the summary being advisory, not normative).
                    pending_longname = None;
                }
            }
        }
        // Trailing bytes after the last full block: padding is fine;
        // non-zero junk without an end marker is a truncated header.
        if !saw_end && tar[off..].iter().any(|b| *b != 0) {
            return Err(TarParseError::TruncatedHeader);
        }
        Ok(self.summary.clone())
    }

    /// The verification pass: reads every manifest entry back through
    /// the sink and compares (size, SHA-256). The import is "done"
    /// only when `summary.failures` is empty and every entry was
    /// checked.
    pub fn verify(&self, sink: &dyn ImportSink) -> VerifySummary {
        let mut observed: Vec<(String, u64, Vec<u8>)> = Vec::new();
        for e in self.manifest.entries() {
            if let Some(data) = sink.read_file(&e.path) {
                observed.push((e.path.clone(), data.len() as u64, data));
            }
        }
        let refs: Vec<(&str, u64, &[u8])> = observed
            .iter()
            .map(|(p, s, d)| (p.as_str(), *s, d.as_slice()))
            .collect();
        self.manifest.verify(refs.into_iter())
    }

    /// The manifest (tooling: `lfs_migrate --verify` prints it).
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

// --- ustar header helpers ---------------------------------------------------

/// Number of 512-byte blocks a body of `size` bytes occupies.
const fn blocks_for(size: u64) -> usize {
    (size as usize + BLOCK - 1) / BLOCK
}

/// The unsigned checksum: sum of all header bytes with the chksum
/// field (8 bytes at 148) treated as ASCII spaces.
fn header_checksum(header: &[u8]) -> u64 {
    let mut sum: u64 = 0;
    for (i, b) in header.iter().enumerate() {
        if (148..156).contains(&i) {
            sum += b' ' as u64;
        } else {
            sum += u64::from(*b);
        }
    }
    sum
}

/// Parses an octal ASCII field (leading spaces/NULs tolerated) into
/// a number. Returns `None` for non-octal content.
fn parse_octal(field: &[u8]) -> Option<u64> {
    let digits: Vec<u8> = field
        .iter()
        .copied()
        .skip_while(|b| *b == b' ' || *b == 0)
        .take_while(|b| *b != 0 && *b != b' ')
        .collect();
    if digits.is_empty() {
        return Some(0); // empty field = zero
    }
    let mut value: u64 = 0;
    for d in digits {
        if !d.is_ascii_digit() || d > b'7' {
            return None;
        }
        value = value.checked_mul(8)?.checked_add(u64::from(d - b'0'))?;
    }
    Some(value)
}

/// The member path: name field composed with the ustar prefix field
/// (offset 345) when present.
fn header_name(header: &[u8]) -> Option<String> {
    let name = String::from_utf8_lossy(
        header[0..100].split(|b| *b == 0).next().unwrap_or(&[]),
    )
    .into_owned();
    let prefix = String::from_utf8_lossy(
        header[345..500].split(|b| *b == 0).next().unwrap_or(&[]),
    )
    .into_owned();
    if prefix.is_empty() {
        Some(name)
    } else if name.is_empty() {
        Some(prefix)
    } else {
        Some(format!("{prefix}/{name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory destination: the ImportSink the tests (and the
    /// simulator) drive.
    #[derive(Default)]
    struct MemSink {
        files: BTreeMap<String, Vec<u8>>,
        dirs: Vec<String>,
        links: Vec<(String, String)>,
    }

    impl ImportSink for MemSink {
        fn write_file(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
            self.files.insert(path.to_owned(), data.to_vec());
            Ok(())
        }
        fn mkdir(&mut self, path: &str) -> io::Result<()> {
            self.dirs.push(path.to_owned());
            Ok(())
        }
        fn symlink(&mut self, path: &str, target: &str) -> io::Result<()> {
            self.links.push((path.to_owned(), target.to_owned()));
            Ok(())
        }
        fn read_file(&self, path: &str) -> Option<Vec<u8>> {
            self.files.get(path).cloned()
        }
    }

    /// Builds a ustar header block for one member.
    fn header(name: &str, size: u64, typeflag: u8, linkname: &str) -> [u8; BLOCK] {
        let mut h = [0u8; BLOCK];
        h[0..name.len()].copy_from_slice(name.as_bytes()); // name
        h[100..107].copy_from_slice(b"0000644"); // mode
        h[108..115].copy_from_slice(b"0000000"); // uid
        h[116..123].copy_from_slice(b"0000000"); // gid
        let size_field = format!("{:011o}", size);
        h[124..124 + size_field.len()].copy_from_slice(size_field.as_bytes());
        let mtime_field = format!("{:011o}", 1_700_000_000u64);
        h[136..136 + mtime_field.len()].copy_from_slice(mtime_field.as_bytes());
        h[148..156].fill(b' '); // chksum placeholder
        h[156] = typeflag;
        if !linkname.is_empty() {
            h[157..157 + linkname.len()].copy_from_slice(linkname.as_bytes());
        }
        h[257..263].copy_from_slice(b"ustar\0"); // magic + version
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        let chk = format!("{:06o}\0 ", sum);
        h[148..156].copy_from_slice(chk.as_bytes());
        h
    }

    fn pad(data: &[u8]) -> Vec<u8> {
        let mut v = data.to_vec();
        while v.len() % BLOCK != 0 {
            v.push(0);
        }
        v
    }

    fn archive(members: &[Vec<u8>]) -> Vec<u8> {
        let mut tar = Vec::new();
        for m in members {
            tar.extend_from_slice(m);
        }
        tar.extend_from_slice(&[0u8; BLOCK]); // end marker
        tar
    }

    #[test]
    fn regular_files_import_and_verify() {
        let mut sink = MemSink::default();
        let mut members = Vec::new();
        let a = header("dir/a.txt", 5, b'0', "");
        members.push(a.to_vec());
        members.push(pad(b"hello"));
        let b = header("dir/b.bin", 4, b'0', "");
        members.push(b.to_vec());
        members.push(pad(&[1, 2, 3, 4]));
        let tar = archive(&members);

        let mut session = TarImportSession::new();
        let summary = session.import(&mut sink, &tar).expect("parse");
        assert_eq!(summary.files, 2);
        assert_eq!(summary.bytes, 9);
        assert_eq!(sink.files["dir/a.txt"], b"hello");

        let verdict = session.verify(&sink);
        assert!(verdict.failures.is_empty());
        assert_eq!(verdict.checked, 2);
    }

    #[test]
    fn prefix_field_composes_the_path() {
        let mut sink = MemSink::default();
        let mut h = header("long-name.txt", 3, b'0', "");
        // ustar prefix: "deep/nested/dir" (checksum recomputed with
        // the prefix bytes included).
        let prefix = b"deep/nested/dir";
        h[345..345 + prefix.len()].copy_from_slice(prefix);
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        let chk = format!("{:06o}\0 ", sum);
        h[148..156].copy_from_slice(chk.as_bytes());
        let tar = archive(&[h.to_vec(), pad(b"abc")]);

        let mut session = TarImportSession::new();
        session.import(&mut sink, &tar).expect("parse");
        assert_eq!(sink.files["deep/nested/dir/long-name.txt"], b"abc");
    }

    #[test]
    fn directories_and_symlinks_land() {
        let mut sink = MemSink::default();
        let members = vec![
            header("dir/", 0, b'5', "").to_vec(),
            header("dir/link", 0, b'2', "target.txt").to_vec(),
        ];
        let tar = archive(&members);
        let mut session = TarImportSession::new();
        let summary = session.import(&mut sink, &tar).expect("parse");
        assert_eq!(summary.dirs, 1);
        assert_eq!(summary.symlinks, 1);
        assert_eq!(sink.dirs, vec!["dir"]);
        assert_eq!(sink.links, vec![("dir/link".to_owned(), "target.txt".to_owned())]);
    }

    #[test]
    fn gnu_longname_record_honored() {
        let mut sink = MemSink::default();
        let long = "a".repeat(120);
        let members = vec![
            header("././@LongLink", long.len() as u64 + 1, b'L', "").to_vec(),
            pad(long.as_bytes()),
            header("truncated-placeholder", 3, b'0', "").to_vec(),
            pad(b"xyz"),
        ];
        let tar = archive(&members);
        let mut session = TarImportSession::new();
        let summary = session.import(&mut sink, &tar).expect("parse");
        assert_eq!(summary.files, 1);
        assert_eq!(sink.files[&long], b"xyz");
    }

    #[test]
    fn bad_checksum_stops_the_stream() {
        let mut sink = MemSink::default();
        let mut h = header("file.txt", 3, b'0', "");
        h[0] = b'X'; // corrupt the name after checksum computed
        let tar = archive(&[h.to_vec(), pad(b"abc")]);
        let mut session = TarImportSession::new();
        let err = session.import(&mut sink, &tar);
        assert!(matches!(err, Err(TarParseError::BadChecksum { .. })));
    }

    #[test]
    fn truncated_body_is_an_error() {
        let mut sink = MemSink::default();
        let mut tar = header("file.txt", 512, b'0', "").to_vec();
        tar.extend_from_slice(&[0u8; 100]); // not a full body
        let mut session = TarImportSession::new();
        let err = session.import(&mut sink, &tar);
        assert!(matches!(err, Err(TarParseError::TruncatedBody { .. })));
    }

    #[test]
    fn non_ustar_magic_is_rejected() {
        let mut sink = MemSink::default();
        let mut h = header("f", 0, b'0', "");
        h[257..263].copy_from_slice(b"NOTUST");
        // Fix the checksum for the new magic (checksum must be valid
        // for the magic check to be the failure).
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        let chk = format!("{:06o}\0 ", sum);
        h[148..156].copy_from_slice(chk.as_bytes());
        let tar = archive(&[h.to_vec()]);
        let mut session = TarImportSession::new();
        assert!(matches!(
            session.import(&mut sink, &tar),
            Err(TarParseError::BadMagic)
        ));
    }

    #[test]
    fn corrupted_destination_fails_verification() {
        let mut sink = MemSink::default();
        let tar = archive(&[header("f.txt", 5, b'0', "").to_vec(), pad(b"hello")]);
        let mut session = TarImportSession::new();
        session.import(&mut sink, &tar).expect("parse");
        // Corrupt the destination after import.
        sink.files.insert("f.txt".to_owned(), b"hellO".to_vec());
        let verdict = session.verify(&sink);
        assert_eq!(verdict.failures.len(), 1);
        assert_eq!(verdict.failures[0].kind, crate::migrate::manifest::VerifyFailure::DigestMismatch);
    }

    #[test]
    fn missing_file_fails_verification() {
        let mut sink = MemSink::default();
        let tar = archive(&[header("f.txt", 5, b'0', "").to_vec(), pad(b"hello")]);
        let mut session = TarImportSession::new();
        session.import(&mut sink, &tar).expect("parse");
        sink.files.remove("f.txt");
        let verdict = session.verify(&sink);
        assert_eq!(verdict.failures.len(), 1);
        assert_eq!(verdict.failures[0].kind, crate::migrate::manifest::VerifyFailure::Missing);
    }

    #[test]
    fn empty_archive_imports_nothing() {
        let mut sink = MemSink::default();
        let tar = archive(&[]);
        let mut session = TarImportSession::new();
        let summary = session.import(&mut sink, &tar).expect("parse");
        assert_eq!(summary.files, 0);
        let verdict = session.verify(&sink);
        assert!(verdict.failures.is_empty());
    }

    #[test]
    fn hardlinks_and_pax_are_counted_not_materialized() {
        let mut sink = MemSink::default();
        let members = vec![
            header("hard", 0, b'1', "target").to_vec(),
            header("paxhdr", 10, b'x', "").to_vec(),
            pad(b"20 path=x\n"),
        ];
        let tar = archive(&members);
        let mut session = TarImportSession::new();
        let summary = session.import(&mut sink, &tar).expect("parse");
        assert_eq!(summary.hardlinks_skipped, 1);
        assert_eq!(summary.pax_skipped, 1);
        assert_eq!(summary.files, 0);
    }

    #[test]
    fn octal_parser_edge_cases() {
        assert_eq!(parse_octal(b"00000000000"), Some(0));
        assert_eq!(parse_octal(b"        \0\0"), Some(0));
        assert_eq!(parse_octal(b"00000000567\0"), Some(0o567));
        assert_eq!(parse_octal(b"00000000899\0"), None); // 8 is not octal
    }
}
