//! Address mapping and parity math for RAID0/1/5/6/10.
//!
//! Design note on RAID5/6 write cost: this implementation always
//! recomputes parity from a full read of the other data chunks in a stripe
//! row, rather than the smaller "read old data + old parity, XOR out old,
//! XOR in new" update real RAID controllers use. That optimization needs
//! the old contents on hand; recomputing from a full-row read is strictly
//! more I/O but is simpler to get right without a way to test it end to
//! end here, and it's the same total data volume a first write to a stripe
//! needs anyway. Trading some write throughput for a version that's easier
//! to verify by inspection was the deliberate choice.

use crate::ondisk::serialization::BLOCK_SIZE;
use crate::pool::gf256;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaidProfile {
    Single = 0,
    Raid0 = 1,
    Raid1 = 2,
    Raid5 = 5,
    Raid6 = 6,
    Raid10 = 10,
}

impl RaidProfile {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => RaidProfile::Raid0,
            2 => RaidProfile::Raid1,
            5 => RaidProfile::Raid5,
            6 => RaidProfile::Raid6,
            10 => RaidProfile::Raid10,
            _ => RaidProfile::Single,
        }
    }

    /// Minimum number of devices this profile needs to make sense.
    pub fn min_devices(&self) -> usize {
        match self {
            RaidProfile::Single => 1,
            RaidProfile::Raid0 => 2,
            RaidProfile::Raid1 => 2,
            RaidProfile::Raid5 => 3,
            RaidProfile::Raid6 => 4,
            RaidProfile::Raid10 => 4,
        }
    }
}

/// Where a single logical block's data lives, and (for parity profiles)
/// what else is needed to keep parity correct on a write.
#[derive(Debug, Clone)]
pub struct StripeLayout {
    /// Block offset within *every* device for this stripe row -- parity
    /// and data devices in the same row all use this same physical offset.
    pub phys_block: u64,
    /// Every device holding a copy of this logical block's data (more than
    /// one only for RAID1/RAID10 mirrors; reads can use any of them,
    /// writes must go to all of them).
    pub data_devs: Vec<usize>,
    /// This block's column index within its stripe row (RAID5/6 only).
    /// Fixed per (row, device) pair -- it does NOT depend on which device
    /// happens to be writing, which matters because Q's coefficient `g^i`
    /// for a given device must be the same no matter which device in the
    /// row triggered the parity recompute. Unused (0) outside RAID5/6.
    pub column: usize,
    /// Parity device(s) for this stripe row: empty outside RAID5/6, one
    /// device for RAID5, two (P and Q, in that order) for RAID6.
    pub parity_devs: Vec<usize>,
    /// The *other* data devices in this same stripe row (not `data_devs`,
    /// not `parity_devs`) paired with their own stable column index, in
    /// the same units as `column`. Each shares `phys_block`.
    pub other_data: Vec<(usize, usize)>, // (device_idx, column)
}

pub struct RaidEngine {
    pub profile: RaidProfile,
    pub chunk_size_blocks: u32,
    pub num_devices: usize,
}

impl RaidEngine {
    /// Recommended chunk size (Phase 2). Rationale, stated explicitly
    /// instead of a bare hardcoded 8:
    ///
    /// 1. 128 KiB is the de-facto "optimal I/O" granularity on modern
    ///    storage (mdadm defaults to 512 KiB chunks but 128 KiB is the
    ///    common optimum for parity RAID with 4 KiB filesystem blocks;
    ///    Btrfs uses a 128 KiB-ish stripe target too). At 4 KiB blocks
    ///    that is 32 blocks.
    /// 2. The chunk is aligned DOWN to a whole number of the device's
    ///    logical sectors when a sector size is known, so chunk writes
    ///    never straddle sector boundaries.
    ///
    /// This is a best-effort heuristic, not a guarantee: SSDs do not
    /// expose erase-block sizes, and the real optimum is workload- and
    /// device-specific. `--chunk` on mkfs_lfs overrides it.
    pub fn recommended_chunk_size_blocks(sector_size: u32) -> u32 {
        let mut chunk = 32u64; // 128 KiB at 4 KiB blocks
        if sector_size > 0 {
            let chunk_bytes = chunk * BLOCK_SIZE as u64;
            let sec = sector_size as u64;
            let rem = chunk_bytes % sec;
            if rem != 0 {
                // Shrink by whole blocks until sector-aligned.
                let shrink_blocks = ((rem + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64).max(1);
                chunk = chunk.saturating_sub(shrink_blocks);
            }
        }
        (chunk.max(1)) as u32
    }

    pub fn new(profile: RaidProfile, chunk_size_blocks: u32, num_devices: usize) -> Self {
        // A chunk size of 0 would divide by zero below; every real profile
        // other than Single stripes, so default to a sane 1 rather than
        // letting a bad config panic deep inside address math.
        let chunk_size_blocks = if chunk_size_blocks == 0 {
            1
        } else {
            chunk_size_blocks
        };
        Self {
            profile,
            chunk_size_blocks,
            num_devices,
        }
    }

    fn simple_stripe(&self, logical_block: u64, devices: usize) -> (usize, u64) {
        let stripe_idx = logical_block / self.chunk_size_blocks as u64;
        let offset_in_stripe = logical_block % self.chunk_size_blocks as u64;
        let dev_idx = (stripe_idx % devices as u64) as usize;
        let physical_block =
            (stripe_idx / devices as u64) * self.chunk_size_blocks as u64 + offset_in_stripe;
        (dev_idx, physical_block)
    }

    /// Full layout for a logical block, valid for every profile. RAID0/1/10
    /// only ever populate `data_devs`; RAID5/6 additionally populate
    /// `column`/`parity_devs`/`other_data`.
    pub fn layout(&self, logical_block: u64) -> StripeLayout {
        match self.profile {
            RaidProfile::Single => StripeLayout {
                phys_block: logical_block,
                data_devs: vec![0],
                column: 0,
                parity_devs: vec![],
                other_data: vec![],
            },
            RaidProfile::Raid0 => {
                let (dev, phys) = self.simple_stripe(logical_block, self.num_devices);
                StripeLayout {
                    phys_block: phys,
                    data_devs: vec![dev],
                    column: 0,
                    parity_devs: vec![],
                    other_data: vec![],
                }
            }
            RaidProfile::Raid1 => StripeLayout {
                phys_block: logical_block,
                data_devs: (0..self.num_devices).collect(),
                column: 0,
                parity_devs: vec![],
                other_data: vec![],
            },
            RaidProfile::Raid10 => {
                // Pairs (0,1), (2,3), ... are mirrors; RAID0-stripe across
                // pairs. num_devices should be even (min_devices() = 4);
                // an odd leftover device is simply not used for striping.
                let pairs = self.num_devices / 2;
                let (pair_idx, phys) = self.simple_stripe(logical_block, pairs.max(1));
                let base = pair_idx * 2;
                StripeLayout {
                    phys_block: phys,
                    data_devs: vec![base, base + 1],
                    column: 0,
                    parity_devs: vec![],
                    other_data: vec![],
                }
            }
            RaidProfile::Raid5 => {
                let data_width = self.num_devices - 1;
                let chunk_idx = logical_block / self.chunk_size_blocks as u64;
                let offset = logical_block % self.chunk_size_blocks as u64;
                let row = chunk_idx / data_width as u64;
                let col = (chunk_idx % data_width as u64) as usize;
                let parity_dev = (row % self.num_devices as u64) as usize;
                // Stable per-row device list (excludes only the parity
                // device), independent of which device is being written --
                // `col`'s meaning (and thus its Q coefficient) never shifts.
                let data_devs_in_row: Vec<usize> =
                    (0..self.num_devices).filter(|d| *d != parity_dev).collect();
                let this_dev = data_devs_in_row[col];
                let others: Vec<(usize, usize)> = data_devs_in_row
                    .iter()
                    .enumerate()
                    .filter(|(c, _)| *c != col)
                    .map(|(c, d)| (*d, c))
                    .collect();
                let phys = row * self.chunk_size_blocks as u64 + offset;
                StripeLayout {
                    phys_block: phys,
                    data_devs: vec![this_dev],
                    column: col,
                    parity_devs: vec![parity_dev],
                    other_data: others,
                }
            }
            RaidProfile::Raid6 => {
                let data_width = self.num_devices - 2;
                let chunk_idx = logical_block / self.chunk_size_blocks as u64;
                let offset = logical_block % self.chunk_size_blocks as u64;
                let row = chunk_idx / data_width as u64;
                let col = (chunk_idx % data_width as u64) as usize;
                let p_dev = (row % self.num_devices as u64) as usize;
                let q_dev = ((row + 1) % self.num_devices as u64) as usize;
                let data_devs_in_row: Vec<usize> = (0..self.num_devices)
                    .filter(|d| *d != p_dev && *d != q_dev)
                    .collect();
                let this_dev = data_devs_in_row[col];
                let others: Vec<(usize, usize)> = data_devs_in_row
                    .iter()
                    .enumerate()
                    .filter(|(c, _)| *c != col)
                    .map(|(c, d)| (*d, c))
                    .collect();
                let phys = row * self.chunk_size_blocks as u64 + offset;
                StripeLayout {
                    phys_block: phys,
                    data_devs: vec![this_dev],
                    column: col,
                    parity_devs: vec![p_dev, q_dev],
                    other_data: others,
                }
            }
        }
    }

    pub fn map_read(&self, logical_block: u64) -> Vec<(usize, u64)> {
        let l = self.layout(logical_block);
        l.data_devs.into_iter().map(|d| (d, l.phys_block)).collect()
    }

    pub fn map_write(&self, logical_block: u64) -> Vec<(usize, u64)> {
        let l = self.layout(logical_block);
        l.data_devs.into_iter().map(|d| (d, l.phys_block)).collect()
    }

    pub fn is_parity_profile(&self) -> bool {
        matches!(self.profile, RaidProfile::Raid5 | RaidProfile::Raid6)
    }
}

/// RAID5 parity: simple XOR across every data block in the row (column
/// order doesn't matter for RAID5 -- P is a plain XOR sum -- only RAID6's Q
/// needs stable columns).
pub fn compute_raid5_parity(blocks: &[&[u8]], block_len: usize) -> Vec<u8> {
    let mut p = vec![0u8; block_len];
    for b in blocks {
        gf256::xor_into(&mut p, b);
    }
    p
}

/// RAID6 P+Q parity. `blocks` is `(column, data)` pairs; column is each
/// block's *stable* position in the stripe row (see `StripeLayout::column`)
/// -- correctness depends on this being consistent every time a given
/// row's parity is recomputed, regardless of which device initiated it.
pub fn compute_raid6_parity(blocks: &[(usize, &[u8])], block_len: usize) -> (Vec<u8>, Vec<u8>) {
    let mut p = vec![0u8; block_len];
    let mut q = vec![0u8; block_len];
    for (col, b) in blocks {
        gf256::xor_into(&mut p, b);
        gf256::mul_xor_into(&mut q, b, gf256::pow(*col as u32));
    }
    (p, q)
}

/// Incremental (read-modify-write) RAID5 P update (Phase 3).
///
/// Math: P = XOR of all data blocks in the row. Rewriting the block at
/// one column changes only its contribution:
///     P_new = P_old XOR D_old XOR D_new
///           = P_old XOR (D_old XOR D_new)
/// i.e. XOR the delta out of parity. Requires P_old to be the parity of
/// the row as it currently sits on disk -- which holds whenever every
/// prior write to the row completed (live single-writer path). NOT
/// idempotent under journal replay of a partially-applied write (where
/// D_on_disk is already D_new but P is still the old row's), so the
/// recovery replay path uses the full recompute instead.
pub fn update_raid5_parity_incremental(old_data: &[u8], new_data: &[u8], old_p: &[u8]) -> Vec<u8> {
    let mut p = old_p.to_vec();
    p.resize(old_data.len(), 0);
    // Zip form (not indexed) so the compiler can autovectorize the
    // three-way XOR into SIMD -- an indexed loop measured ~8 us per
    // 4 KiB block, which made the incremental path a net LOSS on the
    // benchmark despite doing less I/O.
    for ((pv, ov), nv) in p.iter_mut().zip(old_data.iter()).zip(new_data.iter()) {
        *pv ^= ov ^ nv;
    }
    p
}

/// Incremental (read-modify-write) RAID6 P+Q update (Phase 3).
///
/// Math: P is a plain XOR (same as RAID5's). Q = XOR over columns of
/// g^col * D_col, and GF(256) multiplication is linear over XOR, so
/// rewriting column `col`:
///     Q_new = Q_old XOR g^col * (D_old XOR D_new)
/// The delta is scaled by the column's Q coefficient -- the same
/// stable `column` value `compute_raid6_parity` uses, so both paths
/// agree by construction (verified by equivalence tests below).
pub fn update_raid6_parity_incremental(
    column: usize,
    old_data: &[u8],
    new_data: &[u8],
    old_p: &[u8],
    old_q: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    let mut p = old_p.to_vec();
    p.resize(old_data.len(), 0);
    let mut q = old_q.to_vec();
    q.resize(old_data.len(), 0);
    let coeff = gf256::pow(column as u32);
    let table = gf256::mul_table(coeff);
    for (((pv, qv), ov), nv) in p
        .iter_mut()
        .zip(q.iter_mut())
        .zip(old_data.iter())
        .zip(new_data.iter())
    {
        let delta = ov ^ nv;
        *pv ^= delta;
        *qv ^= table[delta as usize];
    }
    (p, q)
}

/// Reconstructs a single missing data block given the surviving data
/// blocks in its row and that row's P parity (RAID5, or RAID6's P-only
/// single-failure case -- both use the same formula since P is a plain
/// XOR either way).
pub fn rebuild_single_from_parity(p: &[u8], surviving: &[&[u8]], block_len: usize) -> Vec<u8> {
    let mut out = p.to_vec();
    out.resize(block_len, 0);
    for b in surviving {
        gf256::xor_into(&mut out, b);
    }
    out
}

/// RAID6 dual-erasure reconstruction: recovers two missing data blocks at
/// column indices `x` and `y` (their position in the Q coefficient
/// sequence, i.e. `g^x`/`g^y`), given the surviving data blocks, the row's
/// real P and Q, and the row's data width (for computing P'/Q' of what
/// survived). Standard two-erasure RAID6 recovery: see H. Peter Anvin,
/// "The mathematics of RAID-6", section on failure recovery.
pub fn rebuild_double_from_parity(
    p: &[u8],
    q: &[u8],
    surviving: &[(usize, &[u8])], // (column index, data)
    x: usize,
    y: usize,
    block_len: usize,
) -> (Vec<u8>, Vec<u8>) {
    let mut p_prime = vec![0u8; block_len];
    let mut q_prime = vec![0u8; block_len];
    for (col, data) in surviving {
        gf256::xor_into(&mut p_prime, data);
        gf256::mul_xor_into(&mut q_prime, data, gf256::pow(*col as u32));
    }
    // A = P xor P' = Dx xor Dy
    let mut a = p.to_vec();
    a.resize(block_len, 0);
    gf256::xor_into(&mut a, &p_prime);
    // B = Q xor Q' = g^x*Dx xor g^y*Dy
    let mut b = q.to_vec();
    b.resize(block_len, 0);
    gf256::xor_into(&mut b, &q_prime);

    let gx = gf256::pow(x as u32);
    let gy = gf256::pow(y as u32);
    let denom = gx ^ gy; // (g^x xor g^y), nonzero since x != y and pow() is injective mod 255
    debug_assert_ne!(
        denom, 0,
        "rebuild_double_from_parity requires distinct column indices"
    );

    // Dx = (B xor g^y * A) / denom, byte-wise.
    let mut dx = b.clone();
    gf256::mul_xor_into(&mut dx, &a, gy);
    for byte in dx.iter_mut() {
        *byte = gf256::div(*byte, denom);
    }
    // Dy = A xor Dx
    let mut dy = a;
    gf256::xor_into(&mut dy, &dx);

    (dx, dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raid0_stripes_across_devices() {
        let e = RaidEngine::new(RaidProfile::Raid0, 4, 3);
        let l0 = e.layout(0);
        let l4 = e.layout(4); // next stripe, should land on device 1
        assert_eq!(l0.data_devs, vec![0]);
        assert_eq!(l4.data_devs, vec![1]);
    }

    #[test]
    fn raid1_mirrors_to_every_device() {
        let e = RaidEngine::new(RaidProfile::Raid1, 0, 3);
        let l = e.layout(42);
        assert_eq!(l.data_devs, vec![0, 1, 2]);
        assert_eq!(l.phys_block, 42);
    }

    #[test]
    fn raid5_rotates_parity_and_excludes_it_from_data() {
        let e = RaidEngine::new(RaidProfile::Raid5, 4, 4); // 4 devices, 3 data + 1 parity per row
                                                           // Row 0 covers chunk indices 0,1,2 (data_width = 3)
        let l0 = e.layout(0);
        assert_eq!(l0.parity_devs, vec![0]); // row 0 % 4 == 0
        assert!(!l0.data_devs.contains(&0));
        assert!(!l0.other_data.iter().any(|(d, _)| *d == l0.data_devs[0]));
        assert!(!l0.other_data.iter().any(|(d, _)| *d == 0));
        // Row 1 (chunk_idx 3,4,5) should rotate parity to device 1
        let l_row1 = e.layout(4 * 3); // chunk_idx = 3 => row 1
        assert_eq!(l_row1.parity_devs, vec![1]);
    }

    #[test]
    fn raid5_parity_roundtrip_single_failure() {
        let block_len = 16;
        let d0 = vec![0xAAu8; block_len];
        let d1 = vec![0x55u8; block_len];
        let d2 = vec![0x0Fu8; block_len];
        let p = compute_raid5_parity(&[&d0, &d1, &d2], block_len);

        // Pretend d1 is lost; rebuild from p, d0, d2.
        let rebuilt = rebuild_single_from_parity(&p, &[&d0, &d2], block_len);
        assert_eq!(rebuilt, d1);
    }

    #[test]
    fn raid6_parity_roundtrip_single_failure_via_p() {
        let block_len = 16;
        let d0 = vec![1u8; block_len];
        let d1 = vec![2u8; block_len];
        let d2 = vec![3u8; block_len];
        let (p, _q) = compute_raid6_parity(&[(0, &d0), (1, &d1), (2, &d2)], block_len);
        let rebuilt = rebuild_single_from_parity(&p, &[&d0, &d2], block_len);
        assert_eq!(rebuilt, d1);
    }

    #[test]
    fn raid6_survives_two_simultaneous_failures() {
        let block_len = 32;
        let mut d0 = vec![0u8; block_len];
        let mut d1 = vec![0u8; block_len];
        let mut d2 = vec![0u8; block_len];
        let mut d3 = vec![0u8; block_len];
        for i in 0..block_len {
            d0[i] = (i * 7 + 1) as u8;
            d1[i] = (i * 13 + 2) as u8;
            d2[i] = (i * 3 + 5) as u8;
            d3[i] = (i * 251 + 9) as u8;
        }
        let (p, q) = compute_raid6_parity(&[(0, &d0), (1, &d1), (2, &d2), (3, &d3)], block_len);

        // Columns 1 and 3 (d1, d3) are "lost"; only 0 and 2 survive.
        let surviving: Vec<(usize, &[u8])> = vec![(0, &d0), (2, &d2)];
        let (rx, ry) = rebuild_double_from_parity(&p, &q, &surviving, 1, 3, block_len);
        assert_eq!(rx, d1);
        assert_eq!(ry, d3);
    }

    // ------------------------------------------------------------------
    // Phase 3: incremental (RMW) parity equivalence tests. The plan
    // requires these to prove the incremental result matches a full
    // recompute for the same inputs BEFORE the incremental path is
    // wired into Disk.
    // ------------------------------------------------------------------

    /// Deterministic xorshift PRNG so failures are reproducible.
    struct TestRng(u64);
    impl TestRng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| (self.next() & 0xFF) as u8).collect()
        }
    }

    #[test]
    fn incremental_raid5_matches_full_recompute_200_rounds() {
        let mut rng = TestRng(0x5eed_5a11);
        let block_len = 4096;
        let columns = 4usize;

        // Start from a consistent row: arbitrary data, parity computed
        // the full way.
        let mut row: Vec<Vec<u8>> = (0..columns).map(|_| rng.bytes(block_len)).collect();
        let refs: Vec<&[u8]> = row.iter().map(|v| v.as_slice()).collect();
        let mut p = compute_raid5_parity(&refs, block_len);

        // 200 rounds: rewrite one random column with new random data,
        // update parity incrementally, then independently recompute the
        // full parity and require equality. Also exercises chains
        // (parity updated from parity updated from parity...).
        for round in 0..200 {
            let col = (rng.next() as usize) % columns;
            let new_data = rng.bytes(block_len);
            let old_data = row[col].clone();

            let p_new = update_raid5_parity_incremental(&old_data, &new_data, &p);

            // Mutate the row the "real" way.
            row[col] = new_data;
            let refs: Vec<&[u8]> = row.iter().map(|v| v.as_slice()).collect();
            let p_full = compute_raid5_parity(&refs, block_len);

            assert_eq!(
                p_new, p_full,
                "round {}: incremental P != full recompute",
                round
            );
            p = p_new;
        }
    }

    #[test]
    fn incremental_raid6_matches_full_recompute_200_rounds() {
        let mut rng = TestRng(0x6eed_6a12);
        let block_len = 4096;
        let columns = 6usize;

        let mut row: Vec<Vec<u8>> = (0..columns).map(|_| rng.bytes(block_len)).collect();
        let mut q = {
            let cols: Vec<(usize, &[u8])> = row
                .iter()
                .enumerate()
                .map(|(i, v)| (i, v.as_slice()))
                .collect();
            let (_p, q) = compute_raid6_parity(&cols, block_len);
            q
        };
        let mut p = {
            let cols: Vec<(usize, &[u8])> = row
                .iter()
                .enumerate()
                .map(|(i, v)| (i, v.as_slice()))
                .collect();
            let (p, _q) = compute_raid6_parity(&cols, block_len);
            p
        };

        for round in 0..200 {
            let col = (rng.next() as usize) % columns;
            let new_data = rng.bytes(block_len);
            let old_data = row[col].clone();

            let (p_new, q_new) = update_raid6_parity_incremental(col, &old_data, &new_data, &p, &q);

            row[col] = new_data;
            let cols: Vec<(usize, &[u8])> = row
                .iter()
                .enumerate()
                .map(|(i, v)| (i, v.as_slice()))
                .collect();
            let (p_full, q_full) = compute_raid6_parity(&cols, block_len);

            assert_eq!(
                p_new, p_full,
                "round {}: incremental P != full recompute",
                round
            );
            assert_eq!(
                q_new, q_full,
                "round {}: incremental Q != full recompute",
                round
            );
            p = p_new;
            q = q_new;
        }
    }

    #[test]
    fn incremental_from_all_zero_row_is_trivially_correct() {
        // First write to a fresh (all-zero) row: old data and old parity
        // are zeros, so the incremental path must produce exactly the
        // full-recompute parity of the new data alone.
        let block_len = 512;
        let zeros = vec![0u8; block_len];
        let new_data: Vec<u8> = (0..block_len).map(|i| (i * 7 + 3) as u8).collect();

        let p = update_raid5_parity_incremental(&zeros, &new_data, &zeros);
        assert_eq!(p, new_data, "P of new_data XOR zeros is new_data itself");

        let (p6, q6) = update_raid6_parity_incremental(3, &zeros, &new_data, &zeros, &zeros);
        let cols = vec![(3usize, new_data.as_slice())];
        let (p_full, q_full) = compute_raid6_parity(&cols, block_len);
        assert_eq!(p6, p_full);
        assert_eq!(q6, q_full);
    }
}
