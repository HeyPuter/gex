#!/bin/bash
# on edger under bench.sh: Debian's arm64 python3.13 running pybench.py in
# the browser runtime under node (tier 1, ELF providers on: the page's
# defaults), with libz.so.1 translated (noauto) and replaced by the wasm
# provider installed at /usr/lib/gex/aarch64/libz.so.1.wasm (found by
# soname). ABBA, REPS rounds; one JSON line per run plus its wall ms.
#   run-py.sh TREE OUT REPS   (OUT: build.sh's directory; DEBIAN_ROOT)
set -u
TREE=$1 O=$2 REPS=${3:-2}
C=${BENCH_CORES%%,*}
R=${DEBIAN_ROOT:?}
H=$(cd "$(dirname "$0")" && pwd)
one() { # mode
  local e=(GEX_SHARED=1)
  [ $1 = translated ] && e+=(GEX_WASM_DLLS=noauto)
  local t0=$(date +%s%N)
  local j=$(env "${e[@]}" timeout 900 taskset -c $C \
    node $TREE/shims/browser/js/run-node.mjs --providers --dir /=$R --dir /lib=$R/usr/lib \
    --file /usr/lib/gex/aarch64/libz.so.1.wasm=$O/libz.so.1.wasm --file /t/pybench.py=$H/pybench.py \
    --env PATH=/usr/bin:/bin --env HOME=/ --argv0 /usr/bin/python3.13 \
    $TREE/web/public/gex/seed.shared.wasm $R/usr/bin/python3.13 -I /t/pybench.py $1 2>/dev/null | grep '^{')
  echo "$j wall_ms=$(( ($(date +%s%N) - t0) / 1000000 ))"
}
for r in $(seq $REPS); do
  if [ $((r % 2)) = 1 ]; then one translated; one wasm; else one wasm; one translated; fi
done
