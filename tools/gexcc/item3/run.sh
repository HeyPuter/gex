#!/bin/bash
# under bench.sh: zt (build.sh's test program) on Debian's arm64
# libz translated, and on the LP64 wasm zlib replacing it, at tier 1 (the
# page's default) and with the function JIT, in the browser runtime under
# node. ABBA, REPS rounds; one JSON line per run (median of 5 inside).
#   run.sh TREE OUT REPS   (OUT: build.sh's directory; DEBIAN_ROOT)
set -u
TREE=$1 O=$2 REPS=${3:-3}
C=${BENCH_CORES%%,*}
R=${DEBIAN_ROOT:?}
one() { # mode tier
  local m=$1 t=$2 e=(GEX_SHARED=1)
  [ $m = wasm ] && e+=(GEX_WASM_DLLS=libz.so.1=/usr/lib/gex/aarch64/libz.so.1.wasm)
  [ $t = fjit ] && e+=(GEX_FJIT=1)
  env "${e[@]}" timeout 900 taskset -c $C \
    node $TREE/shims/browser/js/run-node.mjs --dir /=$R --dir /lib=$R/usr/lib \
    --file /usr/lib/gex/aarch64/libz.so.1.wasm=$O/libz.so.1.wasm --file /t/zt=$O/zt --argv0 /t/zt \
    $TREE/web/public/gex/seed.shared.wasm $O/zt $m-$t 2>/dev/null | grep '^{'
}
for t in tier1 fjit; do
  for r in $(seq $REPS); do
    if [ $((r % 2)) = 1 ]; then one translated $t; one wasm $t; else one wasm $t; one translated $t; fi
  done
done
