#!/bin/bash
# The translation cache across loads (docs/gexcc.md §8) in the browser runtime
# under Node (run-node.mjs, GEX_XCACHE): a run restores what the last one
# saved, and a library replaced at the same path -- between runs, or during
# one after the restore and before any process uses it -- is not served from
# the cache: its new code runs.
#
#   tests/gexcc/xcache/run.sh      (web/public/gex has the aarch64 shared
#                                   shim, kernel and seed; the aarch64 musl
#                                   cross toolchain in TOOLCHAIN)
# Counts and output only; no timing.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
TC=${TOOLCHAIN:?set TOOLCHAIN to an aarch64 musl cross toolchain directory}
CC=$TC/bin/aarch64-linux-musl-gcc
WEB=${WEB:-$ROOT/web/public/gex}
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
mkdir -p "$W/root/lib" "$W/root/bin" "$W/xc"
cp "$TC/aarch64-linux-musl/lib/libc.so" "$W/root/lib/ld-musl-aarch64.so.1"
lib() { "$CC" -O1 -shared -fPIC -DVALUE=$1 -o "$W/root/lib/${2:-libt.so}" "$HERE/libt.c"; }
lib 1
"$CC" -O1 -o "$W/root/bin/t" "$HERE/main.c" -L"$W/root/lib" -lt || exit 2
"$CC" -O1 -o "$W/root/bin/swap" "$HERE/swap.c" || exit 2
run() { # [PROGRAM]
  p=${1:-/bin/t}
  GEX_XCACHE=gate GEX_XCACHE_DIR="$W/xc" GEX_XCACHE_MIN=0 GEX_WEB_DIR="$WEB" GEX_SHARED=1 \
    timeout 300 node "$ROOT/shims/browser/js/run-node.mjs" --dir /="$W/root" --argv0 "$p" \
    "$WEB/seed.shared.wasm" "$W/root$p" 2>&1
}
fail=0
check() { # NAME WANT-VALUE WANT-RESTORE(regex) OUTPUT
  if ! grep -aq "^value $2\$" <<<"$4"; then echo "FAIL $1: wanted value $2, got: $(grep -a '^value' <<<"$4")"; fail=1; return; fi
  if ! grep -aEq "$3" <<<"$4"; then echo "FAIL $1: wanted /$3/ in: $(grep -a 'xcache' <<<"$4" | tr '\n' ' ')"; fail=1; return; fi
  echo "ok   $1"
}
expect() { echo "$(( (1000 / 7) * 21 * $1 + (1000 % 7) * ((1000 % 7) - 1) / 2 * $1 + $1 ))"; }
a=$(run); check "cold run saves" "$(expect 1)" 'xcache gate: saved' "$a"
b=$(run); check "warm run restores" "$(expect 1)" 'restored [1-9][0-9]* modules' "$b"
grep -aq 'changed since the save' <<<"$b" && { echo "FAIL warm run: a file it did not touch counted as changed"; fail=1; }
sleep 1.1
lib 2
c=$(run); check "a replaced library is not served stale" "$(expect 2)" 'libt.so changed since the save' "$c"
d=$(run); check "and the next run restores the new one" "$(expect 2)" 'restored [1-9][0-9]* modules' "$d"
sleep 1.1
lib 3 libt.new
# GEX_JIT_DIAG: this run also says when a stale block of a module it did not
# translate (restored here) has the id of one of its own, and that the block
# was judged by its own module's range, never the one the id names. Ids are compiled into
# the code and count from 1 in every run, so the restored modules' ids are
# this run's fresh ones; taking the id for the module (gate 1019f) sent the
# stale block back to JavaScript for ever, and this run hung before exec.
e=$(GEX_JIT_DIAG=1 run /bin/swap); check "a library replaced after the restore is not served stale" "$(expect 3)" 'libt.so changed since the save' "$e"
check "  a restored stale block whose id one of the run's own has is not sent elsewhere" "$(expect 3)" 'whose id [0-9]+ one of its own has too; (not taken for elsewhere|judged by its own range)' "$e"
# The guest's rename stays in that run's filesystem (run-node copies --dir),
# so the next run has the old libt.so again, which the cache (saved with the
# new one) must not serve either.
f=$(run); check "and the next run, back on the old one, is not served the new" "$(expect 2)" 'libt.so changed since the save' "$f"
[ $fail = 0 ] && echo "xcache: PASS" || echo "xcache: FAIL"
exit $fail
