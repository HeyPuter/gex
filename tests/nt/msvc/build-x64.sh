#!/bin/sh
# MSVC-ABI test programs (x64): cxxeh.cpp built for x86-64 with clang-cl and
# lld-link against import libraries made from the -x64.def files here; the
# output is committed as tests/nt/bin/x64/msvc_cxxeh.exe.
set -eu
PATH=/usr/lib/llvm-22/bin:$PATH
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
llvm-dlltool -m i386:x86-64 -d "$here/msvcrt-x64.def" -l "$tmp/msvcrt.lib"
llvm-dlltool -m i386:x86-64 -d "$here/kernel32-x64.def" -l "$tmp/kernel32.lib"
for n in cxxeh; do
    clang-cl --target=x86_64-pc-windows-msvc /O1 /GS- /EHa /c "$here/$n.cpp" /Fo"$tmp/$n.obj"
    lld-link /nodefaultlib /entry:entry /subsystem:console /dynamicbase /Brepro \
        "$tmp/$n.obj" "$tmp/msvcrt.lib" "$tmp/kernel32.lib" /out:"$here/../bin/x64/msvc_$n.exe"
done
