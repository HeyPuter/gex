#!/bin/sh
# Rebuilds tests/nt/bin from tests/nt/src with MinGW or clang (x86-64,
# UCRT). The
# binaries are committed, so running the tests needs no MinGW.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/bin"
# A source may name its compiler command on a `gex-nt-cc:` line (clang for
# __try and __declspec(thread)); the default is MinGW gcc.
for c in "$here"/src/*.c "$here"/src/*.cpp; do
    n=$(basename "${c%.*}")
    cc=$(sed -n 's/^.*gex-nt-cc: *//p' "$c")
    ${cc:-x86_64-w64-mingw32-gcc} -O2 -s -o "$here/bin/$n.exe" "$c"
done
