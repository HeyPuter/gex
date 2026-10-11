#!/bin/bash
# repeat.sh [N]: N (default 100) runs of the embed wasm-JIT leg (jitknob.sh, MONO_WASM_JIT_VERBOSE=1)
# at thresholds 1, 2 and 10, 4 at a time; a run fails on any missing pattern or invalid module.
# Env: OUT, PROV as jitknob.sh. Writes repeat.log in OUT/embed; keeps the output of failed runs.
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-/home/claude/gex-sandbox/scratch/unity/dnc-gc}; export OUT HERE
one() { thr=$1; i=$2; r=$(THR=$thr bash "$HERE/jitknob.sh" r${thr}_$i MONO_WASM_JIT_VERBOSE=1 | head -1)
  f=$OUT/embed/kn-r${thr}_$i.txt; inv=$(grep -a -c '^WASM_JIT_INVALID' "$f")
  echo "$r inv=$inv"; case "$r" in *PASS) [ "$inv" = 0 ] && rm -f "$f";; esac; }
export -f one
for thr in 1 2 10; do for i in $(seq 1 ${1:-100}); do echo $thr $i; done; done | xargs -P4 -n2 bash -c 'one $0 $1' > "$OUT/embed/repeat.log" 2>&1
echo "$(grep -c 'PASS inv=0' "$OUT/embed/repeat.log") of $((3 * ${1:-100})) clean"
