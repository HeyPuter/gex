#!/bin/sh
# MSVC-ABI test programs (i386): clang-cl and lld-link against import
# libraries made from the .def files here; the outputs are committed into
# tests/nt/bin/x86.
set -eu
PATH=/usr/lib/llvm-22/bin:$PATH
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
llvm-dlltool -m i386 -k -d "$here/msvcrt.def" -l "$tmp/msvcrt.lib"
llvm-dlltool -m i386 -k -d "$here/kernel32.def" -l "$tmp/kernel32.lib"
for src in "$here"/*.cpp; do
    n=$(basename "${src%.cpp}")
    clang-cl --target=i686-pc-windows-msvc /O1 /GS- /EHa /c "$src" /Fo"$tmp/$n.obj"
    lld-link /nodefaultlib /entry:entry /subsystem:console /dynamicbase /safeseh:no /Brepro \
        "$tmp/$n.obj" "$tmp/msvcrt.lib" "$tmp/kernel32.lib" /out:"$here/../bin/x86/msvc_$n.exe"
done
