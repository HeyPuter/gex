#!/bin/sh
# Build the GexA64Icall pass plugin for the system clang (LLVM 22).
#   build.sh OUT_DIR   -> OUT_DIR/GexA64Icall.so
set -e
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
LLVM_CONFIG=${LLVM_CONFIG:-$(command -v llvm-config || command -v llvm-config-22)}
clang++ -shared -fPIC -O2 $($LLVM_CONFIG --cxxflags) "$here/GexA64Icall.cpp" -o "$1/GexA64Icall.so"
