#!/bin/sh
# Rebuilds tests/nt/bin from tests/nt/src with MinGW (x86-64, UCRT). The
# binaries are committed, so running the tests needs no MinGW.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/bin"
for c in "$here"/src/*.c; do
    n=$(basename "$c" .c)
    x86_64-w64-mingw32-gcc -O2 -s -o "$here/bin/$n.exe" "$c" $(sed -n 's/^.*gex-nt-cflags: *//p' "$c")
done
