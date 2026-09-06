#!/bin/bash
# Phase 10 (3.4) container reference measurements.
#
# Runs, on THIS 2-vCPU container:
#  1. real fio 3.36 (built from source) job shapes against the
#     container's backing filesystem (overlay) -- the "mature kernel
#     FS in this environment" reference;
#  2. the same job shapes through lfs_ioperf (the LionFS in-process
#     harness: real FileManager pipeline, real journal commits);
#  3. lfs_smpbench --write buffered/durable (the Phase 10 vfs
#     write-back path) and the read path.
#
# Everything lands in benches/results/3.4/ with raw outputs. These are
# NOT mounted-vs-mounted comparisons (no /dev/fuse in this container);
# they are the three measurable paths on identical hardware, honestly
# labeled.
set -u
cd "$(dirname "$0")/.."
FIO=/tmp/fio-fio-3.36/fio
REF=/tmp/fioref
OUT="$(pwd)/benches/results/3.4"
mkdir -p "$REF" "$OUT/fio-container-reference" "$OUT/vfs-smpbench" "$OUT/harness-ioperf"
NPROC=$(nproc)
echo "== container: $(uname -r), ${NPROC} vCPU, backing: overlay ==" | tee "$OUT/environment.txt"
grep -m1 "model name" /proc/cpuinfo | tee -a "$OUT/environment.txt"

# ---------------------------------------------------------------- fio legs
cd "$REF"
run_fio() {
    local name=$1; shift
    # shellcheck disable=SC2086
    "$FIO" --name="$name" --filename="$REF/fseq.bin" --group_reporting=1 \
        --ioengine=psync --runtime=10 --time_based=1 --output-format=json \
        --output="$name.json" "$@" >/dev/null 2>&1
    python3 - "$name" <<'EOF'
import json, sys
name = sys.argv[1]
d = json.load(open(f"{name}.json"))
j = d["jobs"][0]
for kind in ("read", "write"):
    s = j[kind]
    if s["io_bytes"] > 0:
        lat99 = s.get("lat_ns", {}).get("99 Percentile", 0) / 1e6
        print(f"{name}/{kind}: {s['bw_bytes']/2**20:.1f} MiB/s  iops={s['iops']:.0f}  p99lat={lat99:.2f} ms")
EOF
}
echo "--- fio 3.36 on container overlay fs (buffered, psync, 10s legs) ---" | tee "$OUT/fio-container-reference/summary.txt"
run_fio seq-write --rw=write --bs=64k --size=512m          | tee -a "$OUT/fio-container-reference/summary.txt"
run_fio seq-read  --rw=read  --bs=64k --size=512m          | tee -a "$OUT/fio-container-reference/summary.txt"
run_fio rand-read --rw=randread  --bs=4k --size=512m       | tee -a "$OUT/fio-container-reference/summary.txt"
run_fio rand-write --rw=randwrite --bs=4k --size=512m      | tee -a "$OUT/fio-container-reference/summary.txt"
run_fio mixed --rw=randrw --rwmixread=70 --bs=4k --size=512m | tee -a "$OUT/fio-container-reference/summary.txt"
cp -f "$REF"/*.json "$OUT/fio-container-reference/" 2>/dev/null

# ------------------------------------------------------- LionFS vfs writes
cd - >/dev/null
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --bin lfs_smpbench --bin mkfs_lfs --bin lfs_ioperf 2>&1 | grep -E "^error" && exit 1
IMG=/tmp/vfsbench.img
for MODE in buffered durable; do
    rm -f "$IMG"
    ./target/release/mkfs_lfs "$IMG" 96 >/dev/null 2>&1
    for J in 1 2; do
        EXTRA=""
        if [ "$MODE" = durable ]; then EXTRA="--fsync-every 1024"; fi
        echo "--- vfs smpbench write $MODE, jobs=$J ---"
        ./target/release/lfs_smpbench "$IMG" --write "$MODE" $EXTRA --jobs "$J" --seconds 5 2>&1 \
            | grep -E "json:" | tee -a "$OUT/vfs-smpbench/write-${MODE}-jobs${J}.txt"
    done
done
# read path through the shared disk (3.3 methodology, for continuity),
# on a FRESH image so the read set is exactly the prepared files
IMG3=/tmp/readbench.img
rm -f "$IMG3"
./target/release/mkfs_lfs "$IMG3" 96 >/dev/null 2>&1
./target/release/lfs_smpbench --prepare "$IMG3" 4 8 >/dev/null 2>&1
IMG="$IMG3"
for J in 1 2; do
    echo "--- vfs smpbench read (disk layer), jobs=$J ---"
    ./target/release/lfs_smpbench "$IMG" --jobs "$J" --seconds 5 2>&1 \
        | grep -E "json:" | tee -a "$OUT/vfs-smpbench/read-jobs${J}.txt"
    echo "--- vfs smpbench read (VfsOps &self path), jobs=$J ---"
    ./target/release/lfs_smpbench "$IMG" --read-vfs --jobs "$J" --seconds 5 2>&1 \
        | grep -E "json:" | tee -a "$OUT/vfs-smpbench/read-vfs-jobs${J}.txt"
done

# --------------------------------------------------- harness ioperf suite
# lfs_ioperf runs its fixed workload suite (seq64k w/r, rand4k,
# fragments, compression) against a fresh image; shapes match the fio
# reference legs.
IMG2=/tmp/ioperfbench.img
rm -f "$IMG2"
./target/release/mkfs_lfs "$IMG2" 512 >/dev/null 2>&1
echo "--- ioperf fixed suite (seq64k w/r, rand4k, compress) ---"
./target/release/lfs_ioperf --image "$IMG2" --secs 10 --json 2>&1 | tail -40 | tee "$OUT/harness-ioperf/suite.txt"
echo "archived under $OUT"
