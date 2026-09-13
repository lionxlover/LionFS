//! Standalone Reed-Solomon erasure-coded volumes (LionFS 8.0 — the
//! HFS merge, storage-tooling side).
//!
//! The pool has carried a full GF(256) Reed-Solomon codec
//! (`pool::erasure::RsCode`) since 3.0 — verified by its own unit
//! tests but **unwired**: nothing could actually encode or restore a
//! file with it. The cluster plane's `ecc` module brings a second,
//! independently implemented systematic RS codec (M = V·V_top⁻¹) plus
//! the operational patterns (fragment manifests, verify-on-read)
//! learned there.
//!
//! This module makes the local codec USABLE: encode any file into `n`
//! fragment files of which any `k` rebuild the original, with
//! per-fragment CRC32 framing (torn/corrupt fragments are detected,
//! not silently merged), and a verify pass that reports per-fragment
//! health. `lfs_raid` and `lfs_verify` drive it.
//!
//! Both codecs are systematic: the first k fragments ARE the data
//! shards, so the two implementations interoperate on the data plane
//! (cross-validated in the tests below — the merge's dual-codec
//! guarantee).

use crate::pool::erasure::RsCode;
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};

/// Fragment-file magic ("LIONEC" + format version 1).
const FRAGMENT_MAGIC: [u8; 8] = *b"LIONEC\x01\x00";
/// Header size: magic(8) + n(4) + k(4) + original_len(8) + shard_len(8)
/// + index(4) + crc(4).
const HEADER_SIZE: usize = 40;

/// Erasure-coding parameters.
#[derive(Clone, Copy, Debug)]
pub struct EcParams {
    /// Total fragments per stripe (n = k + parity).
    pub n: usize,
    /// Data fragments (the reconstruction minimum).
    pub k: usize,
}

impl EcParams {
    pub fn new(n: usize, k: usize) -> Result<Self> {
        if k == 0 || n < k || n > 255 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("invalid EC parameters: need 1 <= k <= n <= 255, got n={n} k={k}"),
            ));
        }
        Ok(Self { n, k })
    }

    /// How many fragment losses this layout tolerates.
    pub fn tolerates(&self) -> usize {
        self.n - self.k
    }
}

/// One decoded fragment.
#[derive(Clone, Debug)]
pub struct Fragment {
    pub params: EcParams,
    pub original_len: u64,
    pub shard_len: usize,
    pub index: usize,
    pub payload: Vec<u8>,
}

fn u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn u64_le(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Encode `input` into `params.n` fragment files named
/// `{prefix}.{index}.lfrag` inside `dir`. Returns the fragment paths
/// (index order).
pub fn encode_to_dir(
    input: &[u8],
    params: &EcParams,
    dir: &Path,
    prefix: &str,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let shards = encode_shards(input, params)?;
    let mut out = Vec::with_capacity(params.n);
    for (index, shard) in shards.iter().enumerate() {
        let path = dir.join(format!("{prefix}.{index}.lfrag"));
        write_fragment(&path, params, input.len() as u64, index, shard)?;
        out.push(path);
    }
    Ok(out)
}

/// Pure in-memory encode: pad to k equal shards, produce n shards.
pub fn encode_shards(input: &[u8], params: &EcParams) -> Result<Vec<Vec<u8>>> {
    let k = params.k;
    let shard_len = input.len().div_ceil(k).max(1);
    let mut data_shards: Vec<Vec<u8>> = Vec::with_capacity(k);
    for i in 0..k {
        let start = i * shard_len;
        let mut shard = vec![0u8; shard_len];
        if start < input.len() {
            let end = (start + shard_len).min(input.len());
            shard[..end - start].copy_from_slice(&input[start..end]);
        }
        data_shards.push(shard);
    }
    let rs = RsCode::new(params.n, params.k);
    Ok(rs.encode(&data_shards))
}

fn write_fragment(
    path: &Path,
    params: &EcParams,
    original_len: u64,
    index: usize,
    payload: &[u8],
) -> Result<()> {
    let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
    buf.extend_from_slice(&FRAGMENT_MAGIC);
    buf.extend_from_slice(&(params.n as u32).to_le_bytes());
    buf.extend_from_slice(&(params.k as u32).to_le_bytes());
    buf.extend_from_slice(&original_len.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    buf.extend_from_slice(&(index as u32).to_le_bytes());
    buf.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    buf.extend_from_slice(payload);
    std::fs::write(path, &buf)
}

/// Read and CRC-verify one fragment file.
pub fn read_fragment(path: &Path) -> Result<Fragment> {
    let raw = std::fs::read(path)?;
    if raw.len() < HEADER_SIZE || raw[..8] != FRAGMENT_MAGIC {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{}: not a LionFS EC fragment", path.display()),
        ));
    }
    let n = u32_le(&raw[8..12]) as usize;
    let k = u32_le(&raw[12..16]) as usize;
    let original_len = u64_le(&raw[16..24]);
    let shard_len = u64_le(&raw[24..32]) as usize;
    let index = u32_le(&raw[32..36]) as usize;
    let crc = u32_le(&raw[36..40]);
    let params = EcParams::new(n, k)?;
    if index >= n {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{}: fragment index {index} out of range (n={n})", path.display()),
        ));
    }
    let payload = raw[HEADER_SIZE..].to_vec();
    if payload.len() != shard_len {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "{}: fragment length {} != header shard_len {shard_len}",
                path.display(),
                payload.len()
            ),
        ));
    }
    if crc32fast::hash(&payload) != crc {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{}: fragment CRC mismatch (corrupt)", path.display()),
        ));
    }
    Ok(Fragment {
        params,
        original_len,
        shard_len,
        index,
        payload,
    })
}

/// Reconstruct the original bytes from any `k` fragments of an `n`
/// set. The fragments' headers must agree on (n, k, shard_len,
/// original_len).
pub fn reconstruct(fragments: &[Fragment]) -> Result<Vec<u8>> {
    if fragments.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "no fragments given"));
    }
    let params = fragments[0].params;
    let shard_len = fragments[0].shard_len;
    let original_len = fragments[0].original_len;
    for f in fragments {
        if f.params.n != params.n
            || f.params.k != params.k
            || f.shard_len != shard_len
            || f.original_len != original_len
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "fragments disagree on (n, k, shard_len, original_len) — mixed set?",
            ));
        }
    }
    // Dedup indices and count distinct survivors.
    let mut seen = [false; 256];
    let mut surviving: Vec<(usize, Vec<u8>)> = Vec::with_capacity(fragments.len());
    for f in fragments {
        if !seen[f.index] {
            seen[f.index] = true;
            surviving.push((f.index, f.payload.clone()));
        }
    }
    if surviving.len() < params.k {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "only {} distinct fragments survive; need k={}",
                surviving.len(),
                params.k
            ),
        ));
    }
    let rs = RsCode::new(params.n, params.k);
    let shards = rs.reconstruct(&surviving);
    // Stitch data shards, then trim to the original length.
    let mut out = Vec::with_capacity(shard_len * params.k);
    for shard in shards.iter().take(params.k) {
        out.extend_from_slice(shard);
    }
    if (original_len as usize) > out.len() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "fragment headers promise more bytes than the shards carry",
        ));
    }
    out.truncate(original_len as usize);
    Ok(out)
}

/// Per-fragment verification report.
#[derive(Clone, Debug)]
pub struct VerifyReport {
    pub checked: usize,
    pub healthy: usize,
    pub corrupt: Vec<String>,
}

/// CRC-verify a list of fragment files.
pub fn verify_fragments(paths: &[PathBuf]) -> VerifyReport {
    let mut report = VerifyReport {
        checked: paths.len(),
        healthy: 0,
        corrupt: Vec::new(),
    };
    for p in paths {
        match read_fragment(p) {
            Ok(_) => report.healthy += 1,
            Err(e) => report.corrupt.push(format!("{}: {e}", p.display())),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lionfs_ec_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn params_validation() {
        assert!(EcParams::new(6, 4).is_ok());
        assert!(EcParams::new(4, 4).is_ok(), "k = n (no parity) is valid");
        assert!(EcParams::new(3, 4).is_err(), "k > n is invalid");
        assert!(EcParams::new(0, 0).is_err());
        assert!(EcParams::new(256, 200).is_err(), "RS over GF(256) caps n at 255");
        assert_eq!(EcParams::new(6, 4).unwrap().tolerates(), 2);
    }

    #[test]
    fn roundtrip_survives_every_erasure_pattern() {
        // n=6, k=4: every 2-of-6 loss combination must still rebuild.
        let data = pseudo(100_000, 3);
        let params = EcParams::new(6, 4).unwrap();
        let shards = encode_shards(&data, &params).unwrap();
        assert_eq!(shards.len(), 6);
        // Systematic: the first k shards ARE the padded data.
        let shard_len = 100_000usize.div_ceil(4).max(1);
        for (i, shard) in shards.iter().take(4).enumerate() {
            assert_eq!(shard.len(), shard_len);
            let start = i * shard_len;
            let end = (start + shard_len).min(data.len());
            assert_eq!(&shard[..end - start], &data[start..end]);
        }
        for a in 0..6 {
            for b in (a + 1)..6 {
                let survivors: Vec<(usize, Vec<u8>)> = (0..6)
                    .filter(|&i| i != a && i != b)
                    .map(|i| (i, shards[i].clone()))
                    .collect();
                let rebuilt = reconstruct(
                    &survivors
                        .into_iter()
                        .map(|(i, p)| Fragment {
                            params,
                            original_len: data.len() as u64,
                            shard_len: shard_len,
                            index: i,
                            payload: p,
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap();
                assert_eq!(rebuilt, data, "loss of fragments {{{a},{b}}} must rebuild");
            }
        }
    }

    #[test]
    fn fragment_files_roundtrip_with_losses() {
        let data = pseudo(250_000, 5);
        let params = EcParams::new(8, 5).unwrap();
        let dir = temp_dir("files");
        let paths = encode_to_dir(&data, &params, &dir, "blob").unwrap();
        assert_eq!(paths.len(), 8);
        // Drop any 3 of 8: still rebuild from the remaining 5.
        let drop: Vec<usize> = vec![1, 4, 6];
        let survivors: Vec<PathBuf> = paths
            .iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(_, p)| p.clone())
            .collect();
        let frags: Vec<Fragment> = survivors.iter().map(|p| read_fragment(p).unwrap()).collect();
        let rebuilt = reconstruct(&frags).unwrap();
        assert_eq!(rebuilt, data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_fragments_are_detected() {
        let data = pseudo(50_000, 7);
        let params = EcParams::new(6, 3).unwrap();
        let dir = temp_dir("corrupt");
        let paths = encode_to_dir(&data, &params, &dir, "f").unwrap();
        // Flip one payload byte in fragment 0.
        let raw = std::fs::read(&paths[0]).unwrap();
        let mut tampered = raw.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x80;
        std::fs::write(&paths[0], &tampered).unwrap();
        assert!(read_fragment(&paths[0]).is_err(), "CRC must catch the flip");
        let report = verify_fragments(&paths);
        assert_eq!(report.checked, 6);
        assert_eq!(report.healthy, 5);
        assert_eq!(report.corrupt.len(), 1);
        // And reconstruction works from the untouched five.
        let frags: Vec<Fragment> = paths[1..].iter().map(|p| read_fragment(p).unwrap()).collect();
        assert_eq!(reconstruct(&frags).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unaligned_and_tiny_inputs() {
        for len in [0usize, 1, 7, 4095, 4097, 123_457] {
            let data = pseudo(len, 9);
            let params = EcParams::new(4, 2).unwrap();
            let shards = encode_shards(&data, &params).unwrap();
            let frags: Vec<Fragment> = shards
                .iter()
                .enumerate()
                .map(|(i, p)| Fragment {
                    params,
                    original_len: len as u64,
                    shard_len: p.len(),
                    index: i,
                    payload: p.clone(),
                })
                .collect();
            // Lose one shard of each pair... n=4 k=2: reconstruct from
            // the two PARITY-bearing subsets too.
            let subset: Vec<Fragment> = vec![frags[1].clone(), frags[3].clone()];
            assert_eq!(reconstruct(&subset).unwrap(), data, "len={len}");
            // Zero-length input: shard_len floors at 1.
            if len == 0 {
                assert_eq!(shards[0].len(), 1);
            }
        }
    }

    #[test]
    fn below_k_is_refused() {
        let data = pseudo(1000, 11);
        let params = EcParams::new(6, 4).unwrap();
        let shards = encode_shards(&data, &params).unwrap();
        let frags: Vec<Fragment> = shards
            .iter()
            .take(3) // < k
            .enumerate()
            .map(|(i, p)| Fragment {
                params,
                original_len: data.len() as u64,
                shard_len: p.len(),
                index: i,
                payload: p.clone(),
            })
            .collect();
        let err = reconstruct(&frags).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    /// THE merge test: the local engine's `RsCode` and the cluster
    /// plane's `ecc::RsCodec` are two INDEPENDENT systematic
    /// Reed-Solomon implementations over GF(256). Their data shards
    /// interoperate (both systematic: the first k shards ARE the
    /// data); their parity shards are matrix-convention-specific, so
    /// each codec reconstructs from its own parity — and a set of k
    /// surviving DATA shards reconstructs under EITHER codec.
    #[test]
    fn dual_codec_agreement_with_the_cluster_plane() {
        use lionfs_cluster::ecc::RsCodec;
        let data = pseudo(64 * 1024, 13);
        let (n, k) = (6usize, 4usize);
        let shard_len = data.len().div_ceil(k);

        // Shared systematic data shards (the interop plane).
        let mut data_shards: Vec<Vec<u8>> = Vec::with_capacity(k);
        for i in 0..k {
            let mut s = vec![0u8; shard_len];
            let start = i * shard_len;
            let end = (start + shard_len).min(data.len());
            s[..end - start].copy_from_slice(&data[start..end]);
            data_shards.push(s);
        }

        // Local codec: full systematic encode (n shards: data + local
        // parity) and reconstruction under a 2-erasure pattern.
        let local = RsCode::new(n, k);
        let local_out = local.encode(&data_shards);
        for i in 0..k {
            assert_eq!(
                local_out[i], data_shards[i],
                "local codec must be systematic (data shard {i} verbatim)"
            );
        }
        let surviving: Vec<(usize, Vec<u8>)> = vec![
            (0, local_out[0].clone()),
            (2, local_out[2].clone()),
            (4, local_out[4].clone()),
            (5, local_out[5].clone()),
        ];
        let rebuilt_local = local.reconstruct(&surviving);
        let mut trimmed: Vec<u8> = rebuilt_local.iter().take(k).flatten().copied().collect();
        trimmed.truncate(data.len());
        assert_eq!(trimmed, data, "local codec rebuild under 2 erasures");

        // Cluster codec: its encode returns the m parity shards; its
        // decode reconstructs from data + ITS parity under the same
        // erasure pattern.
        let cluster = RsCodec::new(k, n - k).unwrap();
        let cluster_parity = cluster.encode(&data_shards).unwrap();
        assert_eq!(cluster_parity.len(), n - k);
        // Fill the lost data slot with the cluster codec's OWN parity so
        // the cluster decoder sees a self-consistent set: index 1 lost,
        // parity 3 present (its own), parity 4/5 replaced below.
        let mut cluster_slots: Vec<Option<Vec<u8>>> = data_shards
            .iter()
            .map(|s| Some(s.clone()))
            .collect();
        for p in &cluster_parity {
            cluster_slots.push(Some(p.clone()));
        }
        assert_eq!(cluster_slots.len(), n);
        // Erase {1, 3} on the cluster set and decode.
        cluster_slots[1] = None;
        cluster_slots[3] = None;
        let cluster_rebuilt = cluster.decode(&cluster_slots).unwrap();
        let mut c_trimmed: Vec<u8> = cluster_rebuilt.iter().take(k).flatten().copied().collect();
        c_trimmed.truncate(data.len());
        assert_eq!(c_trimmed, data, "cluster codec rebuild under 2 erasures");

        // THE interop claim: k surviving DATA shards (no parity at all)
        // reconstruct under EITHER codec — the data plane is shared.
        let data_only: Vec<(usize, Vec<u8>)> = data_shards
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.clone()))
            .collect();
        let via_local = local.reconstruct(&data_only);
        let mut l2: Vec<u8> = via_local.iter().take(k).flatten().copied().collect();
        l2.truncate(data.len());
        assert_eq!(l2, data, "data-only survivors work with the local codec");
        let mut data_only_slots: Vec<Option<Vec<u8>>> = vec![None; n];
        for (i, s) in &data_only {
            data_only_slots[*i] = Some(s.clone());
        }
        let via_cluster = cluster.decode(&data_only_slots).unwrap();
        let mut c2: Vec<u8> = via_cluster.iter().take(k).flatten().copied().collect();
        c2.truncate(data.len());
        assert_eq!(c2, data, "data-only survivors work with the cluster codec");
    }
}
