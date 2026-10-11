#!/bin/bash
# on edger under bench.sh: zstt (build-zstd.sh's test program) on Debian's
# arm64 libzstd translated (noauto), and on the LP64 wasm zstd installed at
# /usr/lib/gex/aarch64/libzstd.so.1.wasm (bound by soname), at tier 1 and
# with the function JIT, in the browser runtime under node. ABBA, REPS
# rounds; one JSON line per run (median of 5 inside).
#   run-zstd.sh TREE OUT REPS   (OUT: build-zstd.sh's directory; DEBIAN_ROOT)
set -u
TREE=$1 O=$2 REPS=${3:-2}
C=${BENCH_CORES%%,*}
R=${DEBIAN_ROOT:?}
one() { # mode tier
  local m=$1 t=$2 e=(GEX_SHARED=1)
  [ $m = translated ] && e+=(GEX_WASM_DLLS=noauto)
  [ $t = fjit ] && e+=(GEX_FJIT=1)
  env "${e[@]}" timeout 900 taskset -c $C \
    node $TREE/shims/browser/js/run-node.mjs --dir /=$R --dir /lib=$R/usr/lib \
    --file /usr/lib/gex/aarch64/libzstd.so.1.wasm=$O/libzstd.so.1.wasm --file /t/zstt=$O/zstt --argv0 /t/zstt \
    $TREE/web/public/gex/seed.shared.wasm $O/zstt $m-$t 2>/dev/null | grep '^{'
}
for t in tier1 fjit; do
  for r in $(seq $REPS); do
    if [ $((r % 2)) = 1 ]; then one translated $t; one wasm $t; else one wasm $t; one translated $t; fi
  done
done
