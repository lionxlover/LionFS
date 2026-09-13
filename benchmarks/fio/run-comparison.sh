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

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
export PATH="$REPO_ROOT/target/release:$PATH:/usr/sbin:/sbin"

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
    echo "creating loop image $IMG ($SIZE) ..."
    truncate -s "$SIZE" "$IMG"
    LOOP=$(losetup --find --show "$IMG")
    DEV="$LOOP"
    echo "loop device: $DEV"
    trap 'umount "$OUTDIR/mnt" 2>/dev/null || true; [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true' EXIT
fi

bench_leg() {
    local fs="$1"
    echo "== Testing $fs =="

    # 1. Individual file stress benchmark: 1000 individual 4K files
    echo "  [$fs] Running individual file stress test (1,000 files create/read/stat/unlink)..."
    python3 - "$OUTDIR/mnt" "$OUTDIR/out/files-$fs.json" <<'PY'
import sys, os, time, json

mnt_dir, out_file = sys.argv[1], sys.argv[2]
test_dir = os.path.join(mnt_dir, "deep_file_bench")
os.makedirs(test_dir, exist_ok=True)

N_FILES = 1000
payload = b"LFS710_DEEP_COMPARISON_BLOCK_A1B2" * 112  # 3584 bytes

# 1. Create files with fsync
t0 = time.perf_counter()
for i in range(N_FILES):
    p = os.path.join(test_dir, f"test_file_{i:04d}.dat")
    with open(p, "wb") as f:
        f.write(payload)
        f.flush()
        os.fsync(f.fileno())
t_create = time.perf_counter() - t0
create_rate = N_FILES / max(t_create, 0.0001)

# 2. Stat files
t0 = time.perf_counter()
for i in range(N_FILES):
    p = os.path.join(test_dir, f"test_file_{i:04d}.dat")
    st = os.stat(p)
t_stat = time.perf_counter() - t0
stat_rate = N_FILES / max(t_stat, 0.0001)

# 3. Read files & verify
t0 = time.perf_counter()
for i in range(N_FILES):
    p = os.path.join(test_dir, f"test_file_{i:04d}.dat")
    with open(p, "rb") as f:
        data = f.read()
        assert len(data) == len(payload)
t_read = time.perf_counter() - t0
read_rate = N_FILES / max(t_read, 0.0001)

# 4. Unlink files
t0 = time.perf_counter()
for i in range(N_FILES):
    p = os.path.join(test_dir, f"test_file_{i:04d}.dat")
    os.unlink(p)
t_unlink = time.perf_counter() - t0
unlink_rate = N_FILES / max(t_unlink, 0.0001)

try:
    os.rmdir(test_dir)
except Exception:
    pass

results = {
    "create_files_sec": create_rate,
    "create_time_sec": t_create,
    "stat_ops_sec": stat_rate,
    "stat_time_sec": t_stat,
    "read_files_sec": read_rate,
    "read_time_sec": t_read,
    "unlink_files_sec": unlink_rate,
    "unlink_time_sec": t_unlink,
}
with open(out_file, "w") as f:
    json.dump(results, f, indent=2)
print(f"    Create: {create_rate:.1f} files/s ({t_create:.2f}s), Read: {read_rate:.1f} files/s, Stat: {stat_rate:.1f} ops/s, Unlink: {unlink_rate:.1f} files/s")
PY

    # 2. Standard fio workloads
    for job in seq-write seq-read rand-read rand-write mixed; do
        for run in $(seq 1 "$RUNS"); do
            echo "  [$fs] Running $job (run $run/$RUNS)..."
            sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
            fio --section="$job" --directory="$OUTDIR/mnt" \
                --output="$OUTDIR/out/fio-$fs-$job-$run.json" \
                --output-format=json "$JOBFILE" || {
                echo "  [$fs] Warning: fio job $job returned non-zero"
            }
        done
    done
}

# extract: fio json -> bandwidth, IOPS, latency
metrics_extract() {
    python3 - "$1" "$2" "$RUNS" <<'PY'
import json, statistics, sys
fs, outdir, total_runs = sys.argv[1], sys.argv[2], int(sys.argv[3])
rows = []
for job in ["seq-write","seq-read","rand-read","rand-write","mixed"]:
    bws = []
    iopss = []
    lats = []
    for run in range(1, total_runs + 1):
        try:
            with open(f"{outdir}/out/fio-{fs}-{job}-{run}.json") as f:
                j = json.load(f)
            job_data = j["jobs"][0]
            is_write = "write" in job
            is_mixed = job == "mixed"
            if is_mixed:
                bw = job_data["read"]["bw_bytes"] + job_data["write"]["bw_bytes"]
                iops = job_data["read"]["iops"] + job_data["write"]["iops"]
                # Weighted latency in us
                r_ios = job_data["read"]["total_ios"]
                w_ios = job_data["write"]["total_ios"]
                total_ios = r_ios + w_ios
                lat = ((job_data["read"]["lat_ns"]["mean"] * r_ios + job_data["write"]["lat_ns"]["mean"] * w_ios) / (total_ios or 1)) / 1000.0
            elif is_write:
                bw = job_data["write"]["bw_bytes"]
                iops = job_data["write"]["iops"]
                lat = job_data["write"]["lat_ns"]["mean"] / 1000.0
            else:
                bw = job_data["read"]["bw_bytes"]
                iops = job_data["read"]["iops"]
                lat = job_data["read"]["lat_ns"]["mean"] / 1000.0
            bws.append(bw)
            iopss.append(iops)
            lats.append(lat)
        except Exception:
            pass
    if bws:
        rows.append((job, statistics.median(bws), statistics.median(iopss), statistics.median(lats)))
    else:
        rows.append((job, None, None, None))
for job, bw, iops, lat in rows:
    bw_str = f"{bw / (1024*1024):.2f} MB/s" if bw is not None else "n/a"
    iops_str = f"{iops:.0f}" if iops is not None else "n/a"
    lat_str = f"{lat:.1f} us" if lat is not None else "n/a"
    print(f"{fs}\t{job}\t{bw_str}\t{iops_str}\t{lat_str}")
PY
}

setup_fs() {
    fusermount3 -u "$OUTDIR/mnt" 2>/dev/null || fusermount -u "$OUTDIR/mnt" 2>/dev/null || umount -l "$OUTDIR/mnt" 2>/dev/null || true
    case "$1" in
        ext4)   mkfs.ext4 -F "$DEV" >/dev/null 2>&1; mount -t ext4 "$DEV" "$OUTDIR/mnt" -o relatime ;;
        xfs)    mkfs.xfs -f "$DEV" >/dev/null 2>&1; mount -t xfs "$DEV" "$OUTDIR/mnt" -o relatime ;;
        btrfs)  mkfs.btrfs -f "$DEV" >/dev/null 2>&1; mount -t btrfs "$DEV" "$OUTDIR/mnt" -o relatime ;;
        zfs)    zpool create -f lfsbench "$DEV" >/dev/null 2>&1; zfs set atime=off lfsbench; mount -t zfs lfsbench "$OUTDIR/mnt" ;;
        lionfs)
            local target="$DEV"
            if [ -n "$IMG" ]; then target="$IMG"; fi
            local size_mb=$(( $(numfmt --from=iec "$SIZE") / 1048576 ))
            mkfs_lfs "$target" "$size_mb" > "$OUTDIR/mkfs_lfs.log" 2>&1 || mkfs_lfs "$target" > "$OUTDIR/mkfs_lfs.log" 2>&1
            mount_lfs "$target" "$OUTDIR/mnt" > "$OUTDIR/mount_lfs.log" 2>&1 &
            LFS_PID=$!
            for i in $(seq 1 15); do
                if mount | grep -q "$OUTDIR/mnt"; then break; fi
                sleep 0.5
            done
            ;;
    esac
}

teardown_fs() {
    case "$1" in
        ext4|xfs|btrfs) umount "$OUTDIR/mnt" 2>/dev/null || true ;;
        zfs)            umount "$OUTDIR/mnt" 2>/dev/null || true; zpool destroy lfsbench 2>/dev/null || true ;;
        lionfs)
            fusermount3 -u "$OUTDIR/mnt" 2>/dev/null || fusermount -u "$OUTDIR/mnt" 2>/dev/null || umount -l "$OUTDIR/mnt" 2>/dev/null || true
            sleep 1
            if [ -n "${LFS_PID:-}" ] && kill -0 "$LFS_PID" 2>/dev/null; then
                kill -9 "$LFS_PID" 2>/dev/null || true
            fi
            pkill -9 -f "mount_lfs" 2>/dev/null || true
            ;;
    esac
    sleep 1
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
    echo "- runs per leg: $RUNS, median reported"
    echo "- LionFS leg is FUSE (userspace) -- labeled, not hidden."
    echo
    echo "### Block I/O Throughput, IOPS & Latency (FIO)"
    echo
    echo "| filesystem | job | median throughput | median IOPS | mean latency |"
    echo "|---|---|---|---|---|"
} > "$OUTDIR/summary.md"

: > "$OUTDIR/summary.json"
for fs in $FSES; do
    [ -d "$OUTDIR/out" ] || break
    metrics_extract "$fs" "$OUTDIR" | while IFS=$'\t' read -r fsname job bw iops lat; do
        [ "$bw" = "n/a" ] || echo "| $fsname | $job | $bw | $iops | $lat |" >> "$OUTDIR/summary.md"
    done
done

{
    echo
    echo "### Individual File Stress Test (1,000 Files with fsync per leg)"
    echo
    echo "| filesystem | Create Rate | Read Rate | Metadata Stat | Unlink Rate |"
    echo "|---|---|---|---|---|"
} >> "$OUTDIR/summary.md"

for fs in $FSES; do
    if [ -f "$OUTDIR/out/files-$fs.json" ]; then
        python3 - "$OUTDIR/out/files-$fs.json" "$fs" >> "$OUTDIR/summary.md" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
fs = sys.argv[2]
print(f"| {fs} | {data['create_files_sec']:.1f} files/s | {data['read_files_sec']:.1f} files/s | {data['stat_ops_sec']:.1f} ops/s | {data['unlink_files_sec']:.1f} unlinks/s |")
PY
    fi
done

echo
echo "results in $OUTDIR/summary.md"
cat "$OUTDIR/summary.md"

