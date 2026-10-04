#!/bin/sh
# Build the ICALL pass plugin for the system clang (LLVM 22).
#   tools/wine-wasm/icall/build.sh OUT_DIR   -> OUT_DIR/GexIcall.so
set -e
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
clang++ -shared -fPIC -O2 $(llvm-config --cxxflags) "$here/GexIcall.cpp" -o "$1/GexIcall.so"
