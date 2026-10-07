#!/bin/sh
# Build the GexPoll pass plugin for the system clang (LLVM 22).
#   tools/gexcc/poll/build.sh OUT_DIR   -> OUT_DIR/GexPoll.so
set -e
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
LLVM_CONFIG=${LLVM_CONFIG:-$(command -v llvm-config || command -v llvm-config-22)}
clang++ -shared -fPIC -O2 $($LLVM_CONFIG --cxxflags) "$here/GexPoll.cpp" -o "$1/GexPoll.so"
