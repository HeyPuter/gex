#!/bin/bash
# jitknob.sh NAME [KNOB=V ...]: the embed test's wasm-JIT leg (threshold THR, default 1) with extra env, natively, against
# OUT/provider/mono.wasm (or PROV=); OUT is run.sh's, after a run.sh has built OUT/embed.
# Checks every pattern of run.sh.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:?set OUT to a tools/unity-dotnet/mono build directory}; T=$OUT/embed; N=$1; shift; PROV=${PROV:-$OUT/provider/mono.wasm}
GEXC=${GEXC:-$HERE/../../../target/x86/release/gexc}
E=(); for kv in "$@"; do E+=(--env "$kv"); done
cd $T
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$PROV" timeout 600 $GEXC run --mem64 \
  --env GEXMONO_WASM_JIT=1 --env MONO_WASM_JIT_THRESHOLD=${THR:-1} --env MONO_WASM_JIT_STATS=1 "${E[@]}" \
  --env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" > kn-$N.txt 2>&1
rc=$?
mapfile -t P < <(sed -n '/^PATTERNS=(/,/)$/p' "$HERE/run.sh" | grep -o '"[^"]*"' | tr -d '"')
bad=0; for p in "${P[@]}"; do grep -a -q -x -F "$p" kn-$N.txt || { echo "$N FAIL: no '$p' (rc=$rc)"; bad=1; }; done
[ $bad = 0 ] && echo "$N PASS"
grep -a "\[wasm-jit stats\]\|\[wasm-jit t2\]" kn-$N.txt | tail -2 | cut -c1-200
