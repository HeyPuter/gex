#!/bin/bash
# The page's unit cache and the dense base, across a fork (run-node: the
# page's shim and glue under Node; correctness only). See main.c: process B
# holds Modules process A compiled, whose gex.dense section names A's base; a
# child B forks adopts them and must number them at B's base (5d0fd874). The
# results must match a native run, and GEX_DENSE_DEBUG must log adopts of a
# Module carrying another base (so the case was reached).
#
# With that fix reverted the results still match: adopt numbered its slots and
# the Module's blocks at the same (foreign) base, which is consistent within
# the child, and in this run the child instantiates none of those Modules (it
# misses, and the unit cache serves the units afresh). Only the log check
# fails then. A wrong result needs a foreign range that overlaps one of the
# child's own and both instantiated, which this test does not reach.
#
#   tests/gexcc/unitcache-fork/run.sh   (web/public/gex has the aarch64 shared
#                                        shim, kernel and seed; the aarch64 musl
#                                        cross toolchain in TOOLCHAIN)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
TC=${TOOLCHAIN:?set TOOLCHAIN to an aarch64 musl cross toolchain directory}
CC=$TC/bin/aarch64-linux-musl-gcc
WEB=${WEB:-$ROOT/web/public/gex}
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
mkdir -p "$W/root/lib" "$W/root/bin"
cp "$TC/aarch64-linux-musl/lib/libc.so" "$W/root/lib/ld-musl-aarch64.so.1"
"$CC" -O1 -shared -fPIC -o "$W/root/lib/libfoo.so" "$HERE/libfoo.c" || exit 2
"$HERE/genbar.sh" > "$W/libbar.c"
"$CC" -O1 -shared -fPIC -o "$W/root/lib/libbar.so" "$W/libbar.c" || exit 2
"$CC" -O1 -o "$W/root/bin/t" "$HERE/main.c" -L"$W/root/lib" -lfoo -lbar || exit 2
# The expected values: the same C on the host.
cc -O1 -o "$W/native" "$HERE/main.c" "$HERE/libfoo.c" "$W/libbar.c" -w || exit 2
want=$(cd "$W" && ./native a; ./native b) || true
# GEX_ADOPT_AFTER=0: the child adopts what it inherits at the fork, so the
# Modules from the unit cache meet its numbering for certain (deferred, it may
# translate libfoo afresh on a miss and never adopt).
out=$(GEX_ADOPT_AFTER=${GEX_ADOPT_AFTER:-0} GEX_DENSE_DEBUG=1 GEX_WEB_DIR="$WEB" GEX_SHARED=1 timeout 300 node "$ROOT/shims/browser/js/run-node.mjs" \
  --dir /="$W/root" --argv0 /bin/t "$WEB/seed.shared.wasm" "$W/root/bin/t" 2>&1)
fail=0
for l in "a: bar=" "a: f=" "b: bar=" "b: f=" "child: f=" "child: bar="; do
  w=$(grep -a "^$l" <<<"$want" | head -1)
  grep -aqxF "$w" <<<"$out" || { echo "FAIL: wanted '$w', got '$(grep -a "^$l" <<<"$out" | head -1)'"; fail=1; }
done
grep -aq '^b: child status 0$' <<<"$out" || { echo "FAIL: the child: $(grep -a 'child status' <<<"$out")"; fail=1; }
grep -aq '^launcher: a=0 b=0$' <<<"$out" || { echo "FAIL: $(grep -a '^launcher' <<<"$out")"; fail=1; }
n=$(grep -ac 'adopt: a module compiled elsewhere' <<<"$out")
if [ "$n" = 0 ]; then echo "FAIL: no adopt of a module compiled at another base (the case was not reached)"; fail=1; else echo "ok   $n adopts of a module compiled at another base"; fi
[ -n "${KEEP_LOG:-}" ] && printf '%s\n' "$out" > "$KEEP_LOG"
[ $fail = 0 ] && echo "unitcache-fork: PASS" || echo "unitcache-fork: FAIL"
exit $fail
