# Mounted fio comparison framework (Phase 9, 3.3)

This directory is the executable answer to the oldest honest gap in the
repo: **LionFS has never been measured through a real mount.** Every
throughput number in `benches/` and `docs/benchmarks.md` comes from the
in-process harness (`lfs_ioperf`) — a fast, reproducible, but
*userspace-library* path. This framework runs the same fio jobs against
LionFS-as-mounted (FUSE) and the reference filesystems on the same
device, on whatever hardware you have.

## What this is NOT

- It is **not a table of numbers**. There are zero benchmark results in
  this directory by design: the repo's rule — no number that did not
  come out of a run on stated hardware — applies to the mounted path
  even more strictly than to the harness, because a mounted comparison
  is exactly where fabrication would be most tempting. Every number
  lands in `out/` on the machine that ran it.
- It is not a claim that FUSE LionFS will beat kernel filesystems. The
  harness labels the LionFS leg `lionfs-fuse` everywhere; a FUSE
  data path pays a kernel crossing per operation, and the honest
  expectation is that this shows up as a gap exactly where the
  kernel-integration docs (`docs/kernel_integration.md`) predict it.

## How to run

```bash
# prerequisites: root, fio, mkfs.{ext4,xfs,btrfs}, zfs tools (optional),
# /dev/fuse + fuser3, LionFS binaries on PATH (cargo install --path .)
cd benchmarks/fio
sudo ./run-comparison.sh                    # loop-device image, 8G, 3 runs
sudo ./run-comparison.sh --dev /dev/nvme0n2 --size 32G --runs 5
sudo ./run-comparison.sh --fs "ext4 lionfs" # subset of legs
```

Outputs, under `out/<timestamp>/`:

| file | meaning |
|---|---|
| `fio-<fs>-<job>-<run>.json` | raw fio JSON per leg, per job, per run |
| `summary.md` | the comparison table (median of the runs) |
| `summary.json` | machine-readable form |

## Methodology

- **One device, all legs.** Either a loop image (`truncate` + `losetup`)
  or a real block device via `--dev`. Every filesystem is formatted onto
  that same device, in turn.
- **Identical jobs.** The same `jobs/lionfs-workloads.fio` file drives
  every leg: seq-64k write/read, rand-4k read/write, 70/30 mixed, plus
  a create/unlink metadata storm. `direct=1`, `psync`, `iodepth=16`,
  30 s time-based windows, 1 GiB working set.
- **Identical mount posture.** `relatime`, no vendor-recommended tunings
  (no `noatime,ssd,discard` cherry-picking): the comparison is of
  defaults, because that is what "which filesystem is faster out of the
  box" means.
- **Medians over runs.** 3 runs per leg by default (`--runs 5` for
  publication-grade numbers); the summary reports the median, not the
  best — best-of-N is how marketing numbers happen.
- **LionFS via the real mount path.** `mkfs_lfs` on the image, then
  `mount_lfs` (the FUSE bridge), then fio against the mountpoint, then
  a clean unmount. Whatever the FUSE path costs is what gets measured —
  that is the point.

## Reading the results honestly

When you have run it, three outcomes are all publishable:

1. **LionFS-FUSE within striking distance on sequential work** would
   validate the batching/engine design underneath the bridge (the
   io_uring engine amortizes submission cost that FUSE per-op crossings
   otherwise dominate).
2. **LionFS-FUSE far behind on rand-4k** is the expected shape of a
   userspace filesystem without a kernel-native path; the number to
   compare it against is FUSE-ext4 (add a leg with fuse-overlayfs or
   passthrough if you want a same-bridge baseline).
3. **Anything within noise of zero** means a setup bug (FUSE not
   mounted, `direct=1` on a loop device without `--direct` on the
   loop's backing, etc.) — check the raw JSON before believing any
   table.

Also run `lfs_smpbench` (`tools/smpbench`) for the in-process SMP read
scaling picture; it measures a different question (how the read stack
scales across cores, not the mount bridge).


## Container reference runs (3.4)

A second, smaller script ships beside the real-hardware comparison:
`../run-container-reference.sh`. It exists because the dev container
gained a source-built real fio 3.36 (the `fio` on PATH is the fiona
Python CLI -- not the same program). The script runs the same job
shapes against the container's overlay backing and archives three
honest things under `benches/results/3.4/`:

1. fio legs on the kernel-stack path of THIS container (seq-64k
   w 731 / r 693 MiB/s; rand-4k r 13.7 / w 637 buffered; mixed
   12.4/5.3) -- hardware context, **not** a LionFS mount;
2. `lfs_smpbench` vfs-path SMP numbers (the `&self` surface, one
   shared mount: buffered 1.41x, durable flat, read 1.11x at 2 jobs);
3. the `lfs_ioperf` harness suite on the same box.

None of these three is a mounted-vs-mounted comparison; all three are
reproducible on the stated box. The real comparison on real hardware
remains exactly this directory's `run-comparison.sh`.
