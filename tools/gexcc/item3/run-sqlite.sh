#!/bin/bash
# on edger under bench.sh: Debian's arm64 sqlite3 shell running sq.sql
# (.timer on) in the browser runtime under node, with libsqlite3.so.0
# translated (noauto) and replaced by the LP64 wasm provider installed at
# /usr/lib/gex/aarch64/libsqlite3.so.0.wasm (bound by soname), at tier 1
# and with the function JIT. ABBA, REPS rounds; one JSON line per run: the
# statements' summed real time (ms, sqlite's timer) and the run's wall ms.
#   run-sqlite.sh TREE OUT REPS   (OUT: build-sqlite.sh's directory; DEBIAN_ROOT)
set -u
TREE=$1 O=$2 REPS=${3:-2}
C=${BENCH_CORES%%,*}
R=${DEBIAN_ROOT:?}
H=$(cd "$(dirname "$0")" && pwd)
one() { # mode tier
  local m=$1 t=$2 e=(GEX_SHARED=1)
  [ $m = translated ] && e+=(GEX_WASM_DLLS=noauto)
  [ $t = fjit ] && e+=(GEX_FJIT=1)
  local t0=$(date +%s%N)
  local out=$(env "${e[@]}" timeout 900 taskset -c $C \
    node $TREE/shims/browser/js/run-node.mjs --dir /=$R --dir /lib=$R/usr/lib \
    --file /usr/lib/gex/aarch64/libsqlite3.so.0.wasm=$O/libsqlite3.so.0.wasm --file /t/sq.sql=$H/../../../tests/gexcc/item3/sq.sql \
    --env HOME=/ --argv0 /usr/bin/sqlite3 \
    $TREE/web/public/gex/seed.shared.wasm $R/usr/bin/sqlite3 :memory: ".timer on" ".read /t/sq.sql" 2>/dev/null)
  local wall=$(( ($(date +%s%N) - t0) / 1000000 ))
  local ms=$(echo "$out" | awk '/^Run Time: real/ { s += $4 } END { printf "%.1f", s * 1000 }')
  local res=$(echo "$out" | grep -v '^Run Time' | md5sum | cut -c1-8)
  echo "{\"mode\":\"$m\",\"tier\":\"$t\",\"sql_ms\":$ms,\"wall_ms\":$wall,\"out\":\"$res\"}"
}
for t in tier1 fjit; do
  for r in $(seq $REPS); do
    if [ $((r % 2)) = 1 ]; then one translated $t; one wasm $t; else one wasm $t; one translated $t; fi
  done
done
