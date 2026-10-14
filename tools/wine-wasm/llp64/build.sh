#!/bin/sh
# Build the LLP64 plugin and its macro header (docs/wine-native-x64.md §1.2).
#   tools/wine-wasm/llp64/build.sh OUT_DIR  -> OUT_DIR/GexLLP64.so, OUT_DIR/llp64.h
# clang's C++ headers (libclang-NN-dev) are not installed on our hosts: with no
# CLANG_DEV_INCLUDE, the matching Debian package is fetched once (apt-get
# download, no root) into ~/.cache/gex/libclang-dev/<version>.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(realpath -m "$1"); mkdir -p "$out"
LLVM_CONFIG=${LLVM_CONFIG:-$(command -v llvm-config || command -v llvm-config-22)}
inc=${CLANG_DEV_INCLUDE:-}
if [ -z "$inc" ]; then
  major=$($LLVM_CONFIG --version | cut -d. -f1)
  ver=$(dpkg-query -W -f '${Version}' "libclang-cpp$major" 2>/dev/null || echo unknown)
  cache=$HOME/.cache/gex/libclang-dev/$ver
  inc=$cache/usr/lib/llvm-$major/include
  if [ ! -f "$inc/clang/Basic/TargetInfo.h" ]; then
    tmp=$(mktemp -d)
    (cd "$tmp" && apt-get download "libclang-$major-dev=$ver" >/dev/null)
    mkdir -p "$cache"
    dpkg-deb --fsys-tarfile "$tmp"/*.deb | tar -x -C "$cache" "./usr/lib/llvm-$major/include"
    rm -rf "$tmp"
  fi
fi
# shellcheck disable=SC2046
clang++ -shared -fPIC -O1 -I"$inc" $($LLVM_CONFIG --cxxflags) "$here/GexLLP64.cpp" -o "$out/GexLLP64.so.tmp"
mv "$out/GexLLP64.so.tmp" "$out/GexLLP64.so"
"$here/mkmacros.sh" > "$out/llp64.h"
