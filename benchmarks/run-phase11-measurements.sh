#!/bin/bash
# Phase 11 (3.5) container measurements.
#
# Runs, on THIS 2-vCPU container, the three paths this phase changed:
#  1. lfs_smpbench --write buffered|durable at 1/2 jobs (the P1
#     pipelined txg commit: durable writers must overlap staging with
#     the previous group's I/O now);
#  2. lfs_smpbench read legs (the P2 lock-free reader path);
#  3. lfs_ioperf --snapshot-tax (the P3 O(1) birth-mode snapshot
#     creation + the write tax under a live snapshot);
#  4. the fixed ioperf suite (regression check).
#
# The fio-on-overlay container reference is unchanged by this phase
# (it measures the container's own backing filesystem, not LionFS);
# see benches/results/3.4/fio-container-reference/ for those legs.
#
# Everything lands in benches/results/3.5/ with raw outputs, medians
# reported in summary.txt. No number enters any doc except from here.
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
OUT="$(pwd)/benches/results/3.5"
mkdir -p "$OUT/vfs-smpbench" "$OUT/snapshot-tax" "$OUT/harness-ioperf"

echo "== container: $(uname -r), $(nproc) vCPU, backing: overlay, LionFS 3.5.0 ==" | tee "$OUT/environment.txt"
grep -m1 "model name" /proc/cpuinfo | tee -a "$OUT/environment.txt"

cargo build --release --bin lfs_smpbench --bin lfs_ioperf --bin mkfs_lfs 2>&1 | grep -E "^error" && exit 1

# ------------------------------------------------------- P1: write scaling
IMG=/tmp/vfsbench35.img
for MODE in buffered durable; do
    for J in 1 2; do
        rm -f "$IMG"
        ./target/release/mkfs_lfs "$IMG" 96 >/dev/null 2>&1
        EXTRA=""
        if [ "$MODE" = durable ]; then EXTRA="--fsync-every 1024"; fi
        echo "--- vfs smpbench write $MODE, jobs=$J (3 runs, median below) ---"
        for R in 1 2 3; do
            ./target/release/lfs_smpbench "$IMG" --write "$MODE" $EXTRA --jobs "$J" --seconds 5 2>&1 \
                | grep -E "json:"
        done | tee "$OUT/vfs-smpbench/write-${MODE}-jobs${J}.txt"
        python3 - "$OUT/vfs-smpbench/write-${MODE}-jobs${J}.txt" <<'EOF'
import json, re, sys, statistics
vals = []
for line in open(sys.argv[1]):
    m = re.search(r'json: (.*)$', line)
    if m:
        d = json.loads(m.group(1))
        vals.append(d.get("parallel_mibps", 0))
if vals:
    print(f"  MEDIAN mib_per_s = {statistics.median(vals):.1f} over {len(vals)} runs")
EOF
    done
done

# -------------------------------------------------------- P2: read scaling
IMG2=/tmp/readbench35.img
rm -f "$IMG2"
./target/release/mkfs_lfs "$IMG2" 96 >/dev/null 2>&1
./target/release/lfs_smpbench --prepare "$IMG2" 4 8 >/dev/null 2>&1
for J in 1 2; do
    echo "--- vfs smpbench read (disk layer), jobs=$J ---"
    for R in 1 2 3; do
        ./target/release/lfs_smpbench "$IMG2" --jobs "$J" --seconds 5 2>&1 | grep -E "json:"
    done | tee "$OUT/vfs-smpbench/read-jobs${J}.txt"
    echo "--- vfs smpbench read (VfsOps &self path), jobs=$J ---"
    for R in 1 2 3; do
        ./target/release/lfs_smpbench "$IMG2" --read-vfs --jobs "$J" --seconds 5 2>&1 | grep -E "json:"
    done | tee "$OUT/vfs-smpbench/read-vfs-jobs${J}.txt"
done

# -------------------------------------------- P3: snapshot creation is O(1)
IMG3=/tmp/snapbench35.img
rm -f "$IMG3"
./target/release/mkfs_lfs "$IMG3" 512 >/dev/null 2>&1
echo "--- ioperf snapshot-tax (creation cost + write tax under a live snapshot) ---"
./target/release/lfs_ioperf --image "$IMG3" --snapshot-tax --secs 8 2>&1 \
    | grep -E "snapshot-tax|pattern|snap-" | tee "$OUT/snapshot-tax/run.txt"

# ------------------------------------------------- fixed suite (regression)
IMG4=/tmp/ioperfbench35.img
rm -f "$IMG4"
./target/release/mkfs_lfs "$IMG4" 512 >/dev/null 2>&1
echo "--- ioperf fixed suite (seq64k w/r, rand4k, compress) ---"
./target/release/lfs_ioperf --image "$IMG4" --secs 10 --json 2>&1 | tail -40 | tee "$OUT/harness-ioperf/suite.txt"
echo "archived under $OUT"
