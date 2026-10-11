#!/bin/bash
# on edger under bench.sh, one round: the wasm variants next to the kernels'
# native aarch64 build run translated by gex (tier 1, and the function JIT),
# in the browser shim under node. TREE: the gex tree whose shim runs it.
cd "$(dirname "$0")"
C=${BENCH_CORES%%,*}
T=${TREE:-$(cd ../../.. && pwd)}
for v in ${VARIANTS:-static static-opt lp64 lp64-noopt size64 w64}; do taskset -c $C node harness.mjs $v; done
for t in tier1 fjit; do
  ( cd $T && if [ $t = fjit ]; then export GEX_FJIT=1; fi
    GEX_SHARED=1 timeout 3000 taskset -c $C node shims/browser/js/run-node.mjs web/public/gex/seed.shared.wasm \
      tools/gexcc/bench-codegen/out/kernels.a64 a64-$t 2>/dev/null | grep '^{' )
done
