# The Wired Self-Heal Scrub (3.6)

Status: implemented and wired. Money test:
`phase12_tests::scrub_heals_raid5_bitrot_and_verifies` -- a real
3-device RAID5 pool, one flipped bit, the application read refuses,
one sweep reconstructs from parity, the application reads its
original bytes back.

## What 3.3-3.5 had

A placeholder: a thread that slept, incremented a counter, and never
read a block. The verify body was a comment. The healer was plan-only
(`plan_repair` produced steps nothing executed); `RepairOutcome` had
no producer. The bad-block ledger had no iteration or removal, and
its health report always printed HEALTHY.

## The 3.6 loop

1. **Enumerate** every checksum-tree record (ino, logical ->
   physical, checksum, algorithm, birth). The checksum tree is the
   complete inventory of written data blocks on a checksummed image
   (the mkfs default).
2. **Verify**: read each physical block through the RAID mapping and
   recompute its checksum per the record's OWN algorithm id
   (algorithm agility falls out for free).
3. **Heal** on mismatch (`healer::heal_block_in_place`):
   * RAID5/6: P is the XOR of the data columns at the same phys
     offset, so one corrupted column rebuilds from parity + surviving
     columns at exactly the 4 KiB granularity the checksum covers;
   * RAID1/10: copy from a mirror replica whose bytes verify;
   * the reconstruction is accepted ONLY if it verifies against the
     recorded checksum -- the checksum is the arbiter, so stale or
     wrong parity cannot "repair" a block into different wrong data;
   * the write goes to every device whose copy does not verify, via
     `Disk::write_block_direct` (new): raw positioned I/O, BELOW the
     journal, on purpose -- a repair write is idempotent (a torn one
     leaves the block corrupt, which the next sweep heals again), and
     the checksum tree's physical pointer does not move.
4. **Bookkeep** through the SHARED transaction machinery
   (`SharedCore::stage_and_commit`): verification status back to
   Verified + ledger cleanup on repair; ledger quarantine on loss.
   One writer per image, always -- the scrubber never opens a second
   handle for metadata (the 3.5 placeholder opened its own Disk;
   a writing scrubber must not).

Controls: `.lfs_scrub` (start/pause/resume/stop + status) is
unchanged; `LFS_SCRUB_RATE` (blocks/s, default 256, 0 disables the
thread) and `scrub_sweep_rate(core, rate)` -- the synchronous sweep
tests and tooling drive.

## Honest limits

* Double corruption (P+Q, RAID6) is not executed yet -- the RS(n,k)
  machinery (`pool::erasure::reconstruct`) is the future path; the
  executor returns an error and quarantines instead of pretending.
* Sweep enumeration holds the record list in memory (a 1-TiB image at
  4 KiB blocks is ~16 GiB of records worst case); a paged leaf-chain
  iterator is the follow-up.
* Single-device / RAID0 images cannot heal by definition -- the block
  is quarantined and reported, never silently "repaired".
