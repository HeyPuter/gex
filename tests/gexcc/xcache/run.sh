#!/bin/bash
# The translation cache across loads (docs/gexcc.md §8) in the browser runtime
# under Node (run-node.mjs, GEX_XCACHE): a run restores what the last one
# saved, and a library replaced at the same path between runs is not served
# from the cache -- its new code runs.
#
#   tests/gexcc/xcache/run.sh      (web/public/gex has the aarch64 shared
#                                   shim, kernel and seed; the aarch64 musl
#                                   cross toolchain in TOOLCHAIN)
# Counts and output only; no timing.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
TC=${TOOLCHAIN:-/home/claude/toolchains/aarch64-linux-musl-cross}
CC=$TC/bin/aarch64-linux-musl-gcc
WEB=${WEB:-$ROOT/web/public/gex}
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
mkdir -p "$W/root/lib" "$W/root/bin" "$W/xc"
cp "$TC/aarch64-linux-musl/lib/libc.so" "$W/root/lib/ld-musl-aarch64.so.1"
lib() { "$CC" -O1 -shared -fPIC -DVALUE=$1 -o "$W/root/lib/libt.so" "$HERE/libt.c"; }
lib 1
"$CC" -O1 -o "$W/root/bin/t" "$HERE/main.c" -L"$W/root/lib" -lt || exit 2
run() {
  GEX_XCACHE=gate GEX_XCACHE_DIR="$W/xc" GEX_XCACHE_MIN=0 GEX_WEB_DIR="$WEB" GEX_SHARED=1 \
    timeout 300 node "$ROOT/shims/browser/js/run-node.mjs" --dir /="$W/root" --argv0 /bin/t \
    "$WEB/seed.shared.wasm" "$W/root/bin/t" 2>&1
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
grep -aq 'files unchanged' <<<"$b" && grep -aq ' 0 of ' <<<"$(grep -a 'files unchanged' <<<"$b")" && { echo "FAIL warm run: no file unchanged"; fail=1; }
sleep 1.1
lib 2
c=$(run); check "a replaced library is not served stale" "$(expect 2)" 'libt.so changed since the save' "$c"
d=$(run); check "and the next run restores the new one" "$(expect 2)" 'restored [1-9][0-9]* modules' "$d"
[ $fail = 0 ] && echo "xcache: PASS" || echo "xcache: FAIL"
exit $fail
