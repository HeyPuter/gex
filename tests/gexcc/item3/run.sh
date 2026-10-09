#!/bin/bash
# Gate leg for item 3 (docs/gexcc.md §7.8, §7.13): Debian's zlib, zstd and
# sqlite3 as LP64 wasm libraries replacing the arm64 ones, in the browser
# runtime under node (run-node.mjs: the page's shim and glue). Builds the
# three providers and the test programs zt and zstt, then runs every check
# twice, each time in one guest process tree under a wasm32 shell:
# translated (GEX_WASM_DLLS=noauto) and with the providers found by soname
# at /usr/lib/gex/aarch64. The checks include forks with open streams,
# custom allocators (guest code called from the library), gz* file I/O, and
# zstd's worker threads. sqlite3 covers callbacks both ways, varargs and a
# file database.
#
# Passes when every check's JSON says ok in both modes, all three libraries
# were replaced in the second run, and the sqlite3 shell's output is the
# same in both (bar .stats' memory lines).
#
#   tests/gexcc/item3/run.sh GEXC
# Env: GEXCC_PREFIX (gexcc toolchain: wasm-opt); A64_SYSROOT (the LP64 libc,
# tools/gexcc/item3/sysroot.sh; else built into WORK, which needs
# GEX_CRT_SRC); DEBIAN_ROOT (Debian arm64: usr/lib/aarch64-linux-gnu with
# libz, libzstd, libsqlite3 and usr/bin/sqlite3); SH (a wasm32 sh, default
# the native-debian tree's dash); WORK.
set -u
GEXC=${1:?usage: run.sh GEXC}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
I3=$ROOT/tools/gexcc/item3
R=${DEBIAN_ROOT:-$HOME/gex-sandbox/scratch/CC/archbench/root-arm64}
SH=${SH:-${GEXCC_DEB:-$HOME/gex-sandbox/scratch/CC/debian}/rootfs/tree/usr/bin/dash}
WORK=${WORK:-$(mktemp -d)}; mkdir -p "$WORK"
: "${GEXCC_PREFIX:?set GEXCC_PREFIX}"
for f in "$R/usr/bin/sqlite3" "$R/usr/lib/aarch64-linux-gnu/libsqlite3.so.0" "$SH"; do
  [ -e "$f" ] || { echo "item3: missing $f" >&2; exit 2; }
done
export DEBIAN_ROOT=$R
if [ -z "${A64_SYSROOT:-}" ]; then
  "$I3/sysroot.sh" "$WORK/a64libc" > "$WORK/sysroot.log" 2>&1 || { tail -20 "$WORK/sysroot.log"; exit 1; }
  export A64_SYSROOT=$WORK/a64libc/sysroot
fi
O=$WORK/out
for b in build.sh build-zstd.sh build-sqlite.sh; do
  "$I3/$b" "$O" > "$WORK/$b.log" 2>&1 || { echo "item3: $b failed"; tail -20 "$WORK/$b.log"; exit 1; }
done
SEED=$WORK/seed.shared.wasm
"$GEXC" seed -o "$SEED" >/dev/null || exit 1
mkdir -p "$WORK/tmp"
fail=0
run() { # mode
  local e=(GEX_SHARED=1)
  [ "$1" = translated ] && e+=(GEX_WASM_DLLS=noauto)
  env "${e[@]}" timeout "${TMO:-900}" node "$ROOT/shims/browser/js/run-node.mjs" \
    --dir /=$R --dir /lib=$R/usr/lib --dir /tmp=$WORK/tmp \
    --file /bin/sh=$SH --file /t/checks.sh=$HERE/checks.sh \
    --file /t/sq.sql=$HERE/sq.sql --file /t/cb.sql=$HERE/cb.sql \
    --file /t/zt=$O/zt --file /t/zstt=$O/zstt \
    --file /usr/lib/gex/aarch64/libz.so.1.wasm=$O/libz.so.1.wasm \
    --file /usr/lib/gex/aarch64/libzstd.so.1.wasm=$O/libzstd.so.1.wasm \
    --file /usr/lib/gex/aarch64/libsqlite3.so.0.wasm=$O/libsqlite3.so.0.wasm \
    --argv0 /bin/sh "$SEED" "$SH" /t/checks.sh > "$WORK/$1.out" 2> "$WORK/$1.err"
  echo "item3: $1 exit $?"
}
run translated
run wasm
for m in translated wasm; do
  bad=$(grep '^{' "$WORK/$m.out" | grep -c '"ok":false')
  good=$(grep '^{' "$WORK/$m.out" | grep -c '"ok":true')
  rcs=$(grep '^== rc ' "$WORK/$m.out" | grep -vc '^== rc 0$')
  ran=$(grep -c '^== rc ' "$WORK/$m.out")
  echo "item3: $m: $good checks ok, $bad not, $ran programs, $rcs nonzero exits"
  [ "$bad" = 0 ] && [ "$good" -ge 16 ] && [ "$rcs" = 0 ] && [ "$ran" = 7 ] || fail=1
done
for lib in libz.so.1 libzstd.so.1 libsqlite3.so.0; do
  grep -q "$lib replaced by" "$WORK/wasm.err" || { echo "item3: $lib was not replaced"; fail=1; }
  grep -q "$lib replaced by" "$WORK/translated.err" && { echo "item3: $lib replaced with noauto"; fail=1; }
done
sq() { sed -n '/^== sqlite3 sq.sql/,$p' "$1" | grep -v '^\[\|^Memory Used\|^Number of Outstanding\|^Largest Allocation'; }
if ! diff <(sq "$WORK/translated.out") <(sq "$WORK/wasm.out") > "$WORK/sqlite.diff"; then
  echo "item3: the sqlite3 shell's output differs:"; head -20 "$WORK/sqlite.diff"; fail=1
fi
[ $fail = 0 ] && echo "item3: PASS" || echo "item3: FAIL (outputs in $WORK)"
exit $fail
