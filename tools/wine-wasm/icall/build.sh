#!/bin/sh
# Build the ICALL pass plugin for the system clang (LLVM 22).
#   tools/wine-wasm/icall/build.sh OUT_DIR   -> OUT_DIR/GexIcall.so
set -e
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
LLVM_CONFIG=${LLVM_CONFIG:-$(command -v llvm-config || command -v llvm-config-22)}
clang++ -shared -fPIC -O2 $($LLVM_CONFIG --cxxflags) "$here/GexIcall.cpp" -o "$1/GexIcall.so"
