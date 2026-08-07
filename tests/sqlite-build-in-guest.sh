#!/bin/sh
# Build SQLite from source inside the guest, then run the database it made.
#
#   tests/sqlite-build-in-guest.sh [rootfs-dir]
#
# `tests/gcc-in-guest.sh` proves the toolchain works. This proves it works at
# the scale real software is written at: the SQLite amalgamation is 9 MB and
# 255,680 lines in one translation unit, and compiling it takes `cc1` through
# a quarter of a gigabyte of allocation and several hundred thousand distinct
# basic blocks of its own code.
#
# That scale is the point. The first attempt died with a segmentation fault at
# a guest address, which looked like a miscompile and was not: gex spends one
# wasmtime instance per runtime translation, wasmtime allows ten thousand per
# store by default, and a compile this large wants more. A 900 KB input worked
# and the 9 MB one did not, so it even looked like a size-dependent codegen
# bug. Nothing about the guest was wrong. This test exists so that ceiling --
# or the next one like it -- is found here rather than in a bug report.
#
# The build is slow enough that the suite does not run it by default. Set
# GEX_SLOW=1, or run it directly.
#
# The root filesystem is the one `tests/gcc-in-guest.sh` describes, plus the
# SQLite amalgamation unpacked into /work/sqlite.
set -eu

ROOT="${1:-/tmp/gex-gcc/root}"
GEXC="${GEXC:-target/release/gexc}"

if [ ! -x "$ROOT/usr/bin/gcc" ]; then
    echo "skip: no gcc rootfs at $ROOT (see tests/gcc-in-guest.sh)"
    exit 0
fi
if [ ! -f "$ROOT/work/sqlite/sqlite3.c" ]; then
    echo "skip: no SQLite amalgamation at $ROOT/work/sqlite"
    exit 0
fi
if [ "${GEX_SLOW:-0}" != "1" ] && [ $# -eq 0 ]; then
    echo "skip: sqlite-build-in-guest is slow; set GEX_SLOW=1"
    exit 0
fi

rm -f "$ROOT/work/sqlite/sqlite3" "$ROOT/work/sqlite/out.txt"
cat > "$ROOT/build-sqlite.sh" <<'SCRIPT'
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
cd /work/sqlite
# -O1 rather than -O2: the point is to get through the translation unit, and
# -O2 on nine megabytes buys nothing here but minutes.
gcc -O1 -DSQLITE_OMIT_LOAD_EXTENSION -DSQLITE_THREADSAFE=0 \
    -DSQLITE_ENABLE_MATH_FUNCTIONS \
    shell.c sqlite3.c -o sqlite3 -lm || { echo "COMPILE FAILED"; exit 1; }
ls -l sqlite3 | tr -s ' ' | cut -d' ' -f5,9
# The binary did not exist when this run started, and gex now has to translate
# it too -- machine code produced by translated machine code.
./sqlite3 :memory: \
  "create table t(a integer primary key, b text);
   insert into t(b) values ('one'),('two'),('three');
   select count(*), group_concat(b, '|') from t;
   select sqlite_version() like '3.%';"
SCRIPT

start=$(date +%s)
out=$("$GEXC" run --sysroot "$ROOT" --uid 0 --argv0 /bin/sh \
    "$ROOT/bin/dash" /build-sqlite.sh 2>&1 | grep -v '^\[gex\]')
took=$(( $(date +%s) - start ))

echo "$out" | sed 's/^/     /'
if echo "$out" | grep -q '^3|one|two|three$' && echo "$out" | grep -q '^1$'; then
    echo "ok   sqlite-build-in-guest (${took}s)"
    exit 0
fi
echo "FAIL sqlite-build-in-guest: the guest did not build and run SQLite"
exit 1
