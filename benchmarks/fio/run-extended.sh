#!/usr/bin/env bash
# run-extended.sh — exhaustive multi-vector filesystem benchmark
#
# Tests: ext4, xfs, btrfs, lionfs
# Metrics: 4K/64K/1M block sizes, QD 1/32/128, seq/rand/mixed
# Usage:
#   sudo ./benchmarks/fio/run-extended.sh --dev /dev/nvme0n1p5 --size 7G --runs 2
#   sudo ./benchmarks/fio/run-extended.sh --dev /dev/sdb4 --size 6G --runs 2
#
# Output: benchmarks/fio/out/<timestamp>/extended-summary.md

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
export PATH="$REPO_ROOT/target/release:$PATH:/usr/sbin:/sbin"

DEV=""
RUNS=2
FSES="ext4 xfs btrfs lionfs"
SIZE="7G"
JOBFILE="$SCRIPT_DIR/jobs/extended-comparison.fio"
OUTDIR="$SCRIPT_DIR/out/$(date -u +%Y%m%dT%H%M%SZ)-extended"
IMG=""
LOOP=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dev)    DEV="$2"; shift 2 ;;
        --runs)   RUNS="$2"; shift 2 ;;
        --fs)     FSES="$2"; shift 2 ;;
        --size)   SIZE="$2"; shift 2 ;;
        --jobs)   JOBFILE="$2"; shift 2 ;;
        --outdir) OUTDIR="$2"; shift 2 ;;
        *) echo "unknown flag $1"; exit 1 ;;
    esac
done

command -v fio >/dev/null || { echo "fio not found"; exit 1; }
mkdir -p "$OUTDIR/mnt" "$OUTDIR/out"

if [ -z "$DEV" ]; then
    IMG="$OUTDIR/disk.img"
    echo "Creating loop image $IMG ($SIZE) ..."
    truncate -s "$SIZE" "$IMG"
    LOOP=$(losetup --find --show "$IMG")
    DEV="$LOOP"
    echo "Loop device: $DEV"
    trap 'umount "$OUTDIR/mnt" 2>/dev/null || true; [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true' EXIT
fi

# --- jobs to run (section names in extended-comparison.fio) ---
JOBS=(
    seq-write-64k
    seq-read-64k
    seq-write-1m
    seq-read-1m
    rand-write-4k-qd1
    rand-write-4k-qd32
    rand-write-4k-qd128
    rand-read-4k-qd1
    rand-read-4k-qd32
    rand-read-4k-qd128
    rand-write-64k-qd32
    rand-read-64k-qd32
    mixed-4k-7030
    mixed-4k-5050
)

setup_fs() {
    umount "$OUTDIR/mnt" 2>/dev/null || true
    case "$1" in
        ext4)   mkfs.ext4 -F "$DEV" >/dev/null 2>&1; mount -t ext4 "$DEV" "$OUTDIR/mnt" -o relatime ;;
        xfs)    mkfs.xfs -f "$DEV" >/dev/null 2>&1; mount -t xfs "$DEV" "$OUTDIR/mnt" -o relatime ;;
        btrfs)  mkfs.btrfs -f "$DEV" >/dev/null 2>&1; mount -t btrfs "$DEV" "$OUTDIR/mnt" -o relatime ;;
        lionfs)
            local target="$DEV"
            if [ -n "$IMG" ]; then target="$IMG"; fi
            local size_mb=$(( $(numfmt --from=iec "$SIZE") / 1048576 ))
            mkfs_lfs "$target" "$size_mb" > "$OUTDIR/mkfs_lfs.log" 2>&1 || mkfs_lfs "$target" > "$OUTDIR/mkfs_lfs.log" 2>&1
            mount_lfs "$target" "$OUTDIR/mnt" > "$OUTDIR/mount_lfs.log" 2>&1 &
            LFS_PID=$!
            sleep 4
            ;;
    esac
}

teardown_fs() {
    case "$1" in
        ext4|xfs|btrfs) umount "$OUTDIR/mnt" 2>/dev/null || true; sleep 1 ;;
        lionfs)
            fusermount3 -u "$OUTDIR/mnt" 2>/dev/null || fusermount -u "$OUTDIR/mnt" 2>/dev/null || umount -l "$OUTDIR/mnt" 2>/dev/null || true
            sleep 1
            if [ -n "${LFS_PID:-}" ] && kill -0 "$LFS_PID" 2>/dev/null; then
                kill "$LFS_PID" 2>/dev/null || true
            fi
            sleep 1
            ;;
    esac
}

bench_leg() {
    local fs="$1"
    echo ""
    echo "╔═══════════════════════════════════════╗"
    echo "║  Benchmarking: $fs"
    echo "╚═══════════════════════════════════════╝"
    for job in "${JOBS[@]}"; do
        for run in $(seq 1 "$RUNS"); do
            echo "  [$fs] $job (run $run/$RUNS)..."
            fio --section="$job" --directory="$OUTDIR/mnt" \
                --output="$OUTDIR/out/fio-$fs-$job-$run.json" \
                --output-format=json "$JOBFILE" 2>/dev/null || {
                echo "  [$fs] WARNING: $job returned non-zero"
            }
        done
    done
}

# --- Run all filesystems ---
for fs in $FSES; do
    if [ "$fs" = "lionfs" ] && ! command -v mount_lfs >/dev/null; then
        echo "mount_lfs not found — skipping lionfs"
        continue
    fi
    setup_fs "$fs"
    bench_leg "$fs"
    teardown_fs "$fs"
done

# --- Extract and report results ---
python3 - "$OUTDIR" "$RUNS" "${JOBS[*]}" "$FSES" << 'PY'
import json, statistics, sys, os

outdir, total_runs_str = sys.argv[1], sys.argv[2]
jobs = sys.argv[3].split()
fses = sys.argv[4].split()
total_runs = int(total_runs_str)

rows = []
for fs in fses:
    for job in jobs:
        bws, iopss, lats = [], [], []
        is_write = "write" in job
        is_mixed = "mixed" in job
        for run in range(1, total_runs + 1):
            path = f"{outdir}/out/fio-{fs}-{job}-{run}.json"
            if not os.path.exists(path):
                continue
            try:
                with open(path) as f:
                    j = json.load(f)
                jd = j["jobs"][0]
                if is_mixed:
                    bw = jd["read"]["bw_bytes"] + jd["write"]["bw_bytes"]
                    iops = jd["read"]["iops"] + jd["write"]["iops"]
                    r_ios = jd["read"]["total_ios"]
                    w_ios = jd["write"]["total_ios"]
                    total_ios = r_ios + w_ios
                    lat = ((jd["read"]["lat_ns"]["mean"] * r_ios + jd["write"]["lat_ns"]["mean"] * w_ios)
                           / (total_ios or 1)) / 1000.0
                elif is_write:
                    bw = jd["write"]["bw_bytes"]
                    iops = jd["write"]["iops"]
                    lat = jd["write"]["lat_ns"]["mean"] / 1000.0
                else:
                    bw = jd["read"]["bw_bytes"]
                    iops = jd["read"]["iops"]
                    lat = jd["read"]["lat_ns"]["mean"] / 1000.0
                bws.append(bw); iopss.append(iops); lats.append(lat)
            except Exception as e:
                pass
        if bws:
            rows.append((fs, job,
                          statistics.median(bws),
                          statistics.median(iopss),
                          statistics.median(lats)))
        else:
            rows.append((fs, job, None, None, None))

# Write summary
with open(f"{outdir}/extended-summary.md", "w") as out:
    import datetime
    out.write(f"# Extended Filesystem Benchmark — {datetime.datetime.utcnow().strftime('%Y-%m-%dT%H:%M:%SZ')}\n\n")
    out.write(f"- **Device**: (see run script)\n")
    out.write(f"- **Runs per leg**: {total_runs}, median reported\n")
    out.write(f"- **LionFS**: FUSE userspace mount (labeled, not hidden)\n\n")
    out.write("## Results by Job\n\n")
    out.write("| Filesystem | Job | Throughput | IOPS | Mean Latency |\n")
    out.write("|---|---|---|---|---|\n")
    for fs, job, bw, iops, lat in rows:
        bw_s  = f"{bw / (1024*1024):.2f} MB/s" if bw  is not None else "n/a"
        iops_s = f"{iops:.0f}"                  if iops is not None else "n/a"
        lat_s  = f"{lat:.1f} µs"               if lat  is not None else "n/a"
        out.write(f"| {fs} | {job} | {bw_s} | {iops_s} | {lat_s} |\n")

    # Side-by-side comparison (LionFS vs others) per job
    out.write("\n## Side-by-Side Comparison (LionFS vs Competitors)\n\n")
    headers = ["Job", "ext4", "xfs", "btrfs", "lionfs (FUSE)"]
    avail_fses = fses

    for job in jobs:
        job_rows = {fs: None for fs in avail_fses}
        for fs, j, bw, iops, lat in rows:
            if j == job:
                if bw is not None:
                    job_rows[fs] = f"{bw/(1024*1024):.1f} MB/s / {iops:.0f} IOPS / {lat:.0f} µs"
                else:
                    job_rows[fs] = "n/a"
        out.write(f"### {job}\n\n")
        out.write("| Filesystem | Throughput / IOPS / Latency |\n|---|---|\n")
        for fs in avail_fses:
            val = job_rows.get(fs) or "n/a"
            out.write(f"| {fs} | {val} |\n")
        # winner
        winner_fs, winner_bw = None, 0
        for fs, j, bw, iops, lat in rows:
            if j == job and bw is not None and bw > winner_bw:
                winner_bw = bw; winner_fs = fs
        if winner_fs:
            out.write(f"\n🏆 **Winner**: {winner_fs} ({winner_bw/(1024*1024):.1f} MB/s)\n")
        out.write("\n")

print(f"Summary written to {outdir}/extended-summary.md")
PY

echo ""
echo "=== BENCHMARK COMPLETE ==="
echo "Results: $OUTDIR/extended-summary.md"
cat "$OUTDIR/extended-summary.md"
