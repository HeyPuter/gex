#!/bin/sh
# Rebuilds tests/nt/bin/{x64,x86} from tests/nt/src with MinGW or clang
# (UCRT), for PE32+ and PE32. The binaries are committed, so running the
# tests needs no MinGW.
#
# A source may name its compiler command on a `gex-nt-cc:` line, with ARCH
# for the target's triple prefix (clang for __try and __declspec(thread));
# the default is MinGW gcc.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
for arch in x64 x86; do
    case $arch in x64) triple=x86_64 ;; x86) triple=i686 ;; esac
    mkdir -p "$here/bin/$arch"
    for c in "$here"/src/*.c "$here"/src/*.cpp; do
        n=$(basename "${c%.*}")
        cc=$(sed -n 's/^.*gex-nt-cc: *//p' "$c" | sed "s/ARCH/$triple/g")
        ${cc:-$triple-w64-mingw32-gcc} -O2 -s -o "$here/bin/$arch/$n.exe" "$c"
    done
done
