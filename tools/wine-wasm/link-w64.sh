#!/bin/sh
# Link W64's loader: 64-bit native Wine's Unix side (loader/main.o and
# ntdll's Unix objects, LP64 wasm64 lowered to wasm32) as one closed-world
# program, the ELF64 container gexc runs, flagged as running x86-64 Windows
# code (docs/wine-native-x64.md §5, W64.1). The PE side is Wine's x86_64
# DLLs, translated.
#
#   tools/wine-wasm/link-w64.sh WINE_BUILD OUT   (GEX_WASM64_SYSROOT set)
#
# WINE_BUILD is a wasm64 build tree of the Wine fork (configure
# --host=wasm64-unknown-linux-musl CC=tools/wine-wasm/wcc64
# CXX=tools/wine-wasm/wcc64++); the objects are made there first.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 2 ] || { sed -n 2,12p "$0" >&2; exit 2; }
B=$(cd "$1" && pwd) OUT=$2
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
cd "$B"
objs=$(sed -n '/^dlls\/ntdll\/ntdll.so:/,/^\t/p' Makefile | grep -o 'dlls/ntdll/unix/[a-z_0-9]*\.o' | sort -u |
       grep -v 'signal_arm\|signal_i386\|signal_x86_64\|signal_wasm32')
nice -n 10 make -j"${JOBS:-6}" CXX="$HERE/wcc64++" loader/main.o $objs >&2
mkdir -p gexw64
python3 "$HERE/dlreg.py" ntdll.so:func:__wine_main > gexw64/reg.c
"$HERE/wcc64" -O1 -Wl,--gex-guest-x86-64 -o "$OUT" loader/main.o gexw64/reg.c $objs
