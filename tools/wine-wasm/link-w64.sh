#!/bin/bash
# Link W64's loader: 64-bit native Wine's Unix side (loader/main.o and
# ntdll's Unix objects, LP64 wasm64 lowered to wasm32) as one closed-world
# program, the ELF64 container gexc runs, flagged as running x86-64 Windows
# code (docs/wine-native-x64.md §5, W64.1). The PE side is Wine's x86_64
# DLLs, translated.
#
#   tools/wine-wasm/link-w64.sh WINE_BUILD OUT   (GEX_WASM64_SYSROOT set)
#
# Also the Unix libraries crypt32, ws2_32, msv1_0, dnsapi and nsiproxy (as
# W3's w3/link-loader.sh), registered with dlreg.py.
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
reg="ntdll.so:func:__wine_main"
# The Unix libraries without a display or other dependencies, as W3's
# link-loader.sh links them: each one's objects compiled again with its
# __wine_unix_call_funcs renamed apart, and dlreg's registry naming them.
WF=$(sed -n 's/^srcdir *= *//p' Makefile); WF=$(cd "$WF" && pwd)
for spec in crypt32:crypt32 ws2_32:ws2_32 msv1_0:msv1_0 dnsapi:dnsapi nsiproxy.sys:nsiproxy; do
	dir=${spec%%:*} lib=${spec##*:}
	mkdir -p "gexw64/$lib"
	for c in $(sed -n "/^dlls\/$dir\/$lib.so:/,/^\t/p" Makefile | grep -o "dlls/$dir/[a-z_0-9]*\.o" | sort -u); do
		b=$(basename "$c" .o) out=gexw64/$lib/$(basename "$c")
		[ "$out" -nt "$WF/dlls/$dir/$b.c" ] && [ "$out" -nt "$HERE/link-w64.sh" ] && { objs="$objs $out"; continue; }
		cmd=$(make -n -W "$WF/dlls/$dir/$b.c" "$c" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $c")
		[ -n "$cmd" ] || { echo "link-w64: no compile command for $c" >&2; exit 1; }
		eval "${cmd//-c -o $c/-c -o $out} -D__wine_unix_call_funcs=${lib}__wine_unix_call_funcs \
			-D__wine_unix_call_wow64_funcs=${lib}__wine_unix_call_wow64_funcs" >&2
		objs="$objs $out"
	done
	reg="$reg $lib.so:data:__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
done
python3 "$HERE/dlreg.py" $reg > gexw64/reg.c
"$HERE/wcc64" -O1 -Wl,--gex-guest-x86-64 -o "$OUT" loader/main.o gexw64/reg.c $objs
