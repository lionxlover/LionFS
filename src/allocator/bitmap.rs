use crate::ondisk::serialization::{BlockGroupDescriptor, BLOCK_SIZE};
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

pub struct Allocator;

impl Allocator {
    /// Allocates `count` contiguous blocks within a Block Group.
    /// First-fit scan, starting from the context's allocation frontier
    /// when one exists (Phase 1 cursor), else from the bitmap start.
    pub fn allocate_extents(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        count: u64,
    ) -> Result<u64> {
        if count == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Cannot allocate 0 blocks",
            ));
        }
        let run = match ctx.alloc_cursor {
            Some(cursor) => {
                Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, cursor, count).or_else(
                    || Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, 0, count),
                )
            }
            None => Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, 0, count),
        };
        if let Some(start) = run {
            ctx.alloc_cursor = Some(start + count);
            Ok(start)
        } else {
            let cursor_dbg = ctx.alloc_cursor;
            let free_dbg = Self::count_free_blocks(
                &mut *ctx,
                bg_desc.bg_block_bitmap,
                blocks_per_group as u64,
            )
            .unwrap_or(u64::MAX);
            Err(Error::new(
                ErrorKind::OutOfMemory,
                format!(
                    "PLAIN alloc failed: wanted {} blocks, cursor {:?}, {} free of {}",
                    count, cursor_dbg, free_dbg, blocks_per_group
                ),
            ))
        }
    }

    /// Locality-aware allocation (Phase 1): prefer a free run at or
    /// after `hint` (the caller's last physical block + 1), so a file's
    /// blocks stay physically close together and sequential access
    /// stays sequential. Falls back to a full first-fit scan from the
    /// start of the group when nothing is free at/after the hint -- so
    /// correctness is identical to `allocate_extents`, only the chosen
    /// run differs. Uses `allocator::locality::preferred_search_start`
    /// conventions.
    pub fn allocate_extents_hinted(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        count: u64,
        hint: u64,
    ) -> Result<u64> {
        if count == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Cannot allocate 0 blocks",
            ));
        }
        let clamped_hint =
            crate::allocator::locality::preferred_search_start(Some(hint), blocks_per_group);
        // Order: the caller's locality hint (this file's region) first,
        // then the context-wide frontier cursor, then a full first-fit
        // pass. Every path is correctness-equivalent -- only the chosen
        // free run differs.
        let run = Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, clamped_hint, count)
            .or_else(|| match ctx.alloc_cursor {
                Some(cursor) => {
                    Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, cursor, count)
                }
                None => None,
            })
            .or_else(|| Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, 0, count));
        if let Some(start) = run {
            ctx.alloc_cursor = Some(start + count);
            Ok(start)
        } else {
            let free =
                Self::count_free_blocks(ctx, bg_desc.bg_block_bitmap, blocks_per_group as u64)
                    .unwrap_or(u64::MAX);
            Err(Error::new(ErrorKind::OutOfMemory, format!(
                "No contiguous free space found (wanted {} blocks, hint {:?}, cursor {:?}, {} free of {})",
                count, hint, ctx.alloc_cursor, free, blocks_per_group)))
        }
    }

    /// Metadata allocation (Phase 1): allocates from the END of the
    /// block group, growing downward. Tree nodes (checksum-tree
    /// splits, extent-spill nodes) allocated here stay clear of the
    /// data frontier, so sequential file extents are not fragmented by
    /// interleaved metadata. Does not touch the data cursor. Falls
    /// back to the upper half of the group, then to plain first-fit,
    /// so it can never fail while any free run exists.
    pub fn allocate_extents_meta(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        count: u64,
    ) -> Result<u64> {
        if count == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Cannot allocate 0 blocks",
            ));
        }
        let bpg = blocks_per_group as u64;
        let end_zone_start = bpg.saturating_sub(1 + ctx.meta_high_water);
        let upper_half = bpg / 2;
        let run = Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, end_zone_start, count)
            .or_else(|| {
                Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, upper_half, count)
            })
            .or_else(|| Self::scan_free_run(ctx, bg_desc, blocks_per_group, count, 0, count))
            .ok_or_else(|| {
                let hw = ctx.meta_high_water;
                let free_dbg = Self::count_free_blocks(
                    &mut *ctx,
                    bg_desc.bg_block_bitmap,
                    blocks_per_group as u64,
                )
                .unwrap_or(u64::MAX);
                Error::new(
                    ErrorKind::OutOfMemory,
                    format!(
                        "META alloc failed: wanted {} blocks, high_water {}, {} free of {}",
                        count, hw, free_dbg, blocks_per_group
                    ),
                )
            })?;
        ctx.meta_high_water = (bpg.saturating_sub(run)).max(ctx.meta_high_water) + count - 1;
        Ok(run)
    }

    /// Speculative allocation (Phase 1): find a free run of at least
    /// `want` blocks but mark only `mark` of them (mark <= want). The
    /// unmarked tail stays FREE in the bitmap -- it is a best-effort
    /// reservation that the next sequential write picks up naturally
    /// (the frontier cursor points into it, so the allocator hands the
    /// reserved blocks out first and the caller's extent merges across
    /// the reservation boundary). If no `want`-sized run exists, degrades
    /// to a `mark`-sized run -- speculation is best-effort, never a
    /// failure. Correctness is unconditional: only marked blocks are
    /// ever owned, and unmarked blocks are ordinary free space that
    /// ANYONE (including the checksum tree) may take.
    pub fn allocate_extents_reserved(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        want: u64,
        mark: u64,
        hint: Option<u64>,
    ) -> Result<u64> {
        if mark == 0 || mark > want {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "reserved allocation needs 0 < mark <= want",
            ));
        }
        let want_run = |ctx: &mut TxContext| -> Option<u64> {
            match (hint, ctx.alloc_cursor) {
                (Some(h), _) => {
                    let clamped = crate::allocator::locality::preferred_search_start(
                        Some(h),
                        blocks_per_group,
                    );
                    Self::scan_free_run(ctx, bg_desc, blocks_per_group, want, clamped, mark)
                        .or_else(|| {
                            Self::scan_free_run(ctx, bg_desc, blocks_per_group, want, 0, mark)
                        })
                }
                (None, Some(c)) => {
                    Self::scan_free_run(ctx, bg_desc, blocks_per_group, want, c, mark).or_else(
                        || Self::scan_free_run(ctx, bg_desc, blocks_per_group, want, 0, mark),
                    )
                }
                (None, None) => Self::scan_free_run(ctx, bg_desc, blocks_per_group, want, 0, mark),
            }
        };
        let start = want_run(ctx)
            .or_else(|| Self::scan_free_run(ctx, bg_desc, blocks_per_group, mark, 0, mark))
            .ok_or_else(|| {
                let free =
                    Self::count_free_blocks(ctx, bg_desc.bg_block_bitmap, blocks_per_group as u64)
                        .unwrap_or(u64::MAX);
                Error::new(
                    ErrorKind::OutOfMemory,
                    format!(
                        "RESERVED alloc failed: want {} mark {} hint {:?}, {} free of {}",
                        want, mark, hint, free, blocks_per_group
                    ),
                )
            })?;
        ctx.alloc_cursor = Some(start + mark);
        Ok(start)
    }

    /// First-fit scan for a `count`-block free run, starting from
    /// `from_bit` (absolute block number). Returns the run start and
    /// marks `mark_count` blocks used (normally == count). Runs never
    /// start before `from_bit`.
    fn scan_free_run(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        count: u64,
        from_bit: u64,
        mark_count: u64,
    ) -> Option<u64> {
        let total_bitmap_blocks = (blocks_per_group as u64).div_ceil(BLOCK_SIZE as u64 * 8);
        let mut buf = [0u8; BLOCK_SIZE];

        let bits_per_bitmap_block = BLOCK_SIZE as u64 * 8;
        // Start the scan at the bitmap block containing `from_bit`, at
        // the exact bit offset; earlier bits in that block are skipped.
        let start_bm_idx =
            (from_bit / bits_per_bitmap_block).min(total_bitmap_blocks.saturating_sub(1));
        let mut start_byte = ((from_bit % bits_per_bitmap_block) / 8) as usize;
        let mut start_bit = (from_bit % 8) as u32;

        let mut current_run = 0u64;
        let mut run_start_block = 0u64;

        for bm_idx in start_bm_idx..total_bitmap_blocks {
            ctx.read_block(bg_desc.bg_block_bitmap + bm_idx, &mut buf)
                .ok()?;
            let mut byte_idx = start_byte;
            let mut first_byte = bm_idx == start_bm_idx;
            while byte_idx < BLOCK_SIZE {
                let byte = buf[byte_idx];
                if byte == 0xFF {
                    current_run = 0;
                    byte_idx += 1;
                    first_byte = false;
                    continue;
                }
                let mut bit_idx = if first_byte { start_bit } else { 0 };
                while bit_idx < 8 {
                    let absolute_block =
                        (bm_idx * bits_per_bitmap_block) + (byte_idx as u64 * 8) + bit_idx as u64;
                    if absolute_block >= blocks_per_group as u64 {
                        break;
                    }
                    if (byte & (1 << bit_idx)) == 0 {
                        if current_run == 0 {
                            run_start_block = absolute_block;
                        }
                        current_run += 1;
                        if current_run == count {
                            Self::mark_blocks_used(
                                ctx,
                                bg_desc.bg_block_bitmap,
                                run_start_block,
                                mark_count,
                            )
                            .ok()?;
                            return Some(run_start_block);
                        }
                    } else {
                        current_run = 0;
                    }
                    bit_idx += 1;
                }
                first_byte = false;
                byte_idx += 1;
            }
            // After the first bitmap block, scan subsequent blocks from bit 0.
            start_byte = 0;
            start_bit = 0;
        }
        None
    }

    pub fn free_extents(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        start: u64,
        count: u64,
    ) -> Result<()> {
        Self::mark_blocks_free(ctx, bg_desc.bg_block_bitmap, start, count)
    }

    /// Public since mkfs and tests need to reserve metadata regions
    /// before general allocation starts.
    pub fn mark_blocks_used(
        ctx: &mut TxContext,
        bitmap_start: u64,
        start: u64,
        count: u64,
    ) -> Result<()> {
        Self::modify_blocks(ctx, bitmap_start, start, count, true)
    }

    /// Count free blocks in the bitmap covering `total_blocks`.
    /// Used by tests and diagnostic tools; not a hot path.
    pub fn count_free_blocks(
        ctx: &mut TxContext,
        bitmap_start: u64,
        total_blocks: u64,
    ) -> Result<u64> {
        let bits_per_block = BLOCK_SIZE as u64 * 8;
        let mut free: u64 = 0;
        let mut buf = [0u8; BLOCK_SIZE];
        let mut remaining = total_blocks;
        let mut bm_idx = 0u64;
        while remaining > 0 {
            ctx.read_block(bitmap_start + bm_idx, &mut buf)?;
            let bits_this_block = remaining.min(bits_per_block);
            for i in 0..bits_this_block {
                let byte_idx = (i / 8) as usize;
                let bit_idx = (i % 8) as u32;
                if buf[byte_idx] & (1 << bit_idx) == 0 {
                    free += 1;
                }
            }
            remaining -= bits_this_block;
            bm_idx += 1;
        }
        Ok(free)
    }

    fn mark_blocks_free(
        ctx: &mut TxContext,
        bitmap_start: u64,
        start: u64,
        count: u64,
    ) -> Result<()> {
        Self::modify_blocks(ctx, bitmap_start, start, count, false)
    }

    fn modify_blocks(
        ctx: &mut TxContext,
        bitmap_start: u64,
        start: u64,
        count: u64,
        set: bool,
    ) -> Result<()> {
        let mut current_bm_idx = u64::MAX;
        let mut buf = [0u8; BLOCK_SIZE];
        let mut modified = false;

        for i in 0..count {
            let block = start + i;
            let bm_idx = block / (BLOCK_SIZE as u64 * 8);
            let byte_idx = ((block % (BLOCK_SIZE as u64 * 8)) / 8) as usize;
            let bit_idx = block % 8;

            if bm_idx != current_bm_idx {
                if modified {
                    ctx.write_block(bitmap_start + current_bm_idx, &buf)?;
                }
                ctx.read_block(bitmap_start + bm_idx, &mut buf)?;
                current_bm_idx = bm_idx;
                modified = false;
            } else {
                // Do nothing
            }

            if set {
                buf[byte_idx] |= 1 << bit_idx;
            } else {
                buf[byte_idx] &= !(1 << bit_idx);
            }
            modified = true;
        }

        if modified {
            ctx.write_block(bitmap_start + current_bm_idx, &buf)?;
        }
        Ok(())
    }
}
