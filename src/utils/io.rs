//! Small `Read`/`Write` helpers.

use std::io::{Read, Result, Write};

/// Reads until `buf` is completely full or EOF, returning the number of
/// bytes actually read (unlike `Read::read_exact`, this does not error on
/// a short read at EOF -- useful when a short final chunk is expected and
/// legitimate, e.g. reading the last, partial block of a file's tail).
pub fn read_up_to(mut r: impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match r.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// Writes `data` in fixed-size chunks, calling `on_chunk` for each one.
/// Used for streaming a large buffer through something that only accepts
/// bounded pieces at a time (e.g. one on-disk block).
pub fn for_each_chunk(
    data: &[u8],
    chunk_size: usize,
    mut on_chunk: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    for chunk in data.chunks(chunk_size.max(1)) {
        on_chunk(chunk)?;
    }
    Ok(())
}

/// Copies `src` into `dst` via `Write`, in `chunk_size` pieces, rather than
/// one large `write_all` call -- useful when `dst` is something like a
/// rate-limited or instrumented writer where large single writes would
/// skew measurements.
pub fn copy_in_chunks(mut src: impl Read, mut dst: impl Write, chunk_size: usize) -> Result<u64> {
    let mut buf = vec![0u8; chunk_size.max(1)];
    let mut total = 0u64;
    loop {
        let n = read_up_to(&mut src, &mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        total += n as u64;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn read_up_to_stops_cleanly_at_eof() {
        let mut cursor = Cursor::new(b"hello".to_vec());
        let mut buf = [0u8; 10];
        let n = read_up_to(&mut cursor, &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn for_each_chunk_covers_all_data() {
        let data = b"abcdefghij";
        let mut collected = Vec::new();
        for_each_chunk(data, 3, |chunk| {
            collected.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap();
        assert_eq!(collected, data);
    }

    #[test]
    fn copy_in_chunks_round_trips() {
        let data = b"the quick brown fox".to_vec();
        let mut out = Vec::new();
        let n = copy_in_chunks(Cursor::new(data.clone()), &mut out, 4).unwrap();
        assert_eq!(n as usize, data.len());
        assert_eq!(out, data);
    }
}
