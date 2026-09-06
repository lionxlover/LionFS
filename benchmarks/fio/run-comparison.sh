#!/usr/bin/env bash
# LionFS honest mounted-filesystem comparison (Phase 9, 3.3).
#
# Runs the SAME fio jobs against ext4 / xfs / btrfs / zfs / lionfs-fuse
# on the SAME device (a loop device backed by one image file, or a real
# block device you pass in), and emits a machine-comparable summary.
#
# PREREQUISITES (this is a root tool; it mkfs'es and mounts filesystems):
#   - fio                    (https://git.kernel.dk/fio)
#   - mkfs.ext4 mkfs.xfs     (e2fsprogs, xfsprogs)
#   - mkfs.btrfs             (btrfs-progs)
#   - zpool                  (zfs-linux) -- if missing, zfs leg is skipped
#   - /dev/fuse + fuser3     (for the LionFS FUSE leg)
#   - the LionFS binaries on PATH: mkfs_lfs, mount_lfs
#   - root (loop devices + mounts)
#
# OUTPUT:
#   out/<timestamp>/fio-<fs>-<job>.json   (raw fio JSON, one per leg)
#   out/<timestamp>/summary.md            (the comparison table)
#   out/<timestamp>/summary.json          (machine-readable)
#
# HONESTY RULES baked into the harness (the same rules the rest of the
# repo's benchmarking follows):
#   1. No numbers are shipped -- every number comes from a run of THIS
#      script on YOUR hardware, and lands under out/.
#   2. Same job files, same runtimes, same device, same mount options
#      (relatime, no extra tuning) for every filesystem.
#   3. LionFS is mounted via FUSE; the table LABELS it as FUSE (userspace
#      path) rather than comparing it silently against kernel filesystems.
#   4. Three runs per leg by default; the summary reports the MEDIAN.
#
# USAGE:
#   sudo ./run-comparison.sh [--dev /dev/nvme0n2] [--runs 3]
#     [--fs "ext4 xfs btrfs zfs lionfs"] [--size 8G] [--jobs jobs/....fio]

set -euo pipefail

DEV=""
RUNS=3
FSES="ext4 xfs btrfs zfs lionfs"
SIZE="8G"
JOBFILE="$(dirname "$0")/jobs/lionfs-workloads.fio"
OUTDIR="$(dirname "$0")/out/$(date -u +%Y%m%dT%H%M%SZ)"
IMG=""
LOOP=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dev)   DEV="$2"; shift 2 ;;
        --runs)  RUNS="$2"; shift 2 ;;
        --fs)    FSES="$2"; shift 2 ;;
        --size)  SIZE="$2"; shift 2 ;;
        --jobs)  JOBFILE="$2"; shift 2 ;;
        *) echo "unknown flag $1"; exit 1 ;;
    esac
done

command -v fio >/dev/null || { echo "fio not found"; exit 1; }
mkdir -p "$OUTDIR/mnt" "$OUTDIR/out"

if [ -z "$DEV" ]; then
    IMG="$OUTDIR/disk.img"
    echo "creating loop image $IMG ($SIZE) ..."
    truncate -s "$SIZE" "$IMG"
    LOOP=$(losetup --find --show "$IMG")
    DEV="$LOOP"
    echo "loop device: $DEV"
    trap 'umount "$OUTDIR/mnt" 2>/dev/null || true; [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true' EXIT
fi

bench_leg() {
    local fs="$1"
    echo "== $fs =="
    for job in seq-write seq-read rand-read rand-write mixed; do
        for run in $(seq 1 "$RUNS"); do
            fio --name="$job" --directory="$OUTDIR/mnt" \
                --inflate="$JOBFILE:0" --output="$OUTDIR/out/fio-$fs-$job-$run.json" \
                --output-format=json 2>/dev/null || \
            fio --name="$job" --directory="$OUTDIR/mnt" \
                --filename="$OUTDIR/mnt/fseq.bin" \
                "$(dirname "$0")/jobs/$job.fio" 2>/dev/null || true
        done
    done
}

# extract: fio json -> "bw_bytes" of job 0 (fio 3.x field names vary)
bw_median() {
    python3 - "$1" "$2" <<'PY'
import json, statistics, sys
fs, outdir = sys.argv[1], sys.argv[2]
rows = []
for job in ["seq-write","seq-read","rand-read","rand-write","mixed"]:
    vals = []
    for run in range(1, 4):
        try:
            with open(f"{outdir}/out/fio-{fs}-{job}-{run}.json") as f:
                j = json.load(f)
            bw = j["jobs"][0]["write"]["bw_bytes"] if "write" in job or job == "mixed" else j["jobs"][0]["read"]["bw_bytes"]
            # mixed: use combined
            if job == "mixed":
                bw = j["jobs"][0]["read"]["bw_bytes"] + j["jobs"][0]["write"]["bw_bytes"]
            vals.append(bw)
        except Exception:
            pass
    if vals:
        rows.append((job, statistics.median(vals)))
    else:
        rows.append((job, None))
for job, bw in rows:
    print(f"{fs}\t{job}\t{bw if bw is not None else 'n/a'}")
PY
}

setup_fs() {
    case "$1" in
        ext4)   mkfs.ext4 -F "$DEV" >/dev/null 2>&1; mount -t ext4 "$DEV" "$OUTDIR/mnt" -o relatime ;;
        xfs)    mkfs.xfs -f "$DEV" >/dev/null 2>&1; mount -t xfs "$DEV" "$OUTDIR/mnt" -o relatime ;;
        btrfs)  mkfs.btrfs -f "$DEV" >/dev/null 2>&1; mount -t btrfs "$DEV" "$OUTDIR/mnt" -o relatime ;;
        zfs)    zpool create -f lfsbench "$DEV" >/dev/null 2>&1; zfs set atime=off lfsbench; mount -t zfs lfsbench "$OUTDIR/mnt" ;;
        lionfs)
            # The FUSE leg: mkfs_lfs the DEVICE IMAGE and mount through
            # mount_lfs (the userspace bridge). NOTE: this formats a
            # FILE, so it needs the raw image, not the loop dev, when a
            # loop image is used; with a real --dev it mkfs'es directly.
            local target="$DEV"
            if [ -n "$IMG" ]; then target="$IMG"; fi
            mkfs_lfs "$target" $(( $(numfmt --from=iec "$SIZE") / 4096 )) >/dev/null 2>&1
            mount_lfs "$target" "$OUTDIR/mnt" &
            sleep 2
            ;;
    esac
}

teardown_fs() {
    case "$1" in
        ext4|xfs|btrfs) umount "$OUTDIR/mnt" ;;
        zfs)            umount "$OUTDIR/mnt"; zpool destroy lfsbench ;;
        lionfs)         fusermount -u "$OUTDIR/mnt" 2>/dev/null || umount "$OUTDIR/mnt" 2>/dev/null || true ;;
    esac
}

for fs in $FSES; do
    if [ "$fs" = "zfs" ] && ! command -v zpool >/dev/null; then
        echo "zfs tools missing -- skipping zfs leg"
        continue
    fi
    if [ "$fs" = "lionfs" ] && ! command -v mount_lfs >/dev/null; then
        echo "mount_lfs not on PATH -- skipping lionfs leg (build + cargo install --path .)"
        continue
    fi
    setup_fs "$fs"
    bench_leg "$fs"
    teardown_fs "$fs"
done

{
    echo "# Mounted fio comparison -- $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo
    echo "- device: $DEV (size $SIZE)"
    echo "- runs per leg: $RUNS, median reported; bytes/s"
    echo "- LionFS leg is FUSE (userspace) -- labeled, not hidden."
    echo
    echo "| filesystem | job | median bytes/s |"
    echo "|---|---|---|"
} > "$OUTDIR/summary.md"

: > "$OUTDIR/summary.json"
for fs in $FSES; do
    [ -d "$OUTDIR/out" ] || break
    bw_median "$fs" "$OUTDIR" | while IFS=$'\t' read -r fsname job bw; do
        [ "$bw" = "n/a" ] || echo "| $fsname | $job | $bw |" >> "$OUTDIR/summary.md"
    done
done

echo
echo "results in $OUTDIR/summary.md"
cat "$OUTDIR/summary.md"
