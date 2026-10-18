#!/bin/bash
# tests/games/stubs/build.sh NAME ARCH OUT: build stub NAME (NAME.c and
# NAME-ARCH.def) for ARCH (i386 or x64) as the DLL OUT, with mingw-w64.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
name=$1 arch=$2 out=$3
case $arch in i386) cc=i686-w64-mingw32-gcc entry=_DllMain@12 ;; x64) cc=x86_64-w64-mingw32-gcc entry=DllMain ;; *) echo "arch is i386 or x64" >&2; exit 2 ;; esac
$cc -O1 -shared -nostdlib -Wl,-e,$entry -Wl,--enable-reloc-section -o "$out" "$here/$name.c" "$here/$name-$arch.def" -Wl,--enable-stdcall-fixup
