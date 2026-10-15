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
# W3's w3/link-loader.sh), registered with dlreg.py, and, when the tree was
# configured with them (W64.4, w64-deps.sh's libraries: W64_DEPS), win32u
# (+ FreeType) and winewayland.drv (+ libwayland, xkbcommon).
# WINE_BUILD is a wasm64 build tree of the Wine fork (configure
# --host=wasm64-unknown-linux-musl CC=tools/wine-wasm/wcc64
# CXX=tools/wine-wasm/wcc64++); the objects are made there first.
set -eu
J=${JOBS:-6}
HERE=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 2 ] || { sed -n 2,12p "$0" >&2; exit 2; }
B=$(cd "$1" && pwd) OUT=$2
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
cd "$B"
objs=$(sed -n '/^dlls\/ntdll\/ntdll.so:/,/^\t/p' Makefile | grep -o 'dlls/ntdll/unix/[a-z_0-9]*\.o' | sort -u |
       grep -v 'signal_arm\|signal_i386\|signal_x86_64\|signal_wasm32')
nice -n 10 make -j"$J" CXX="$HERE/wcc64++" loader/main.o $objs >&2
mkdir -p gexw64
reg="ntdll.so:func:__wine_main"
# The Unix libraries without a display or other dependencies, as W3's
# link-loader.sh links them: each one's objects compiled again with its
# __wine_unix_call_funcs renamed apart, and dlreg's registry naming them.
WF=$(sed -n 's/^srcdir *= *//p' Makefile); WF=$(cd "$WF" && pwd)
fail=gexw64/.fail; rm -f "$fail"
# compile SRC_REL OBJ_REL OUT DEFS: OUT from the build's command for OBJ_REL,
# with DEFS, unless OUT is newer than its source and this script; at most J
# at once.
compile() {
	local src=$WF/$1 rel=$2 out=$3 defs=$4 cmd
	[ -f "$src" ] || src=${src%.c}.cpp  # (win32u's syscall_wasm64.cpp)
	objs="$objs $out"
	[ "$out" -nt "$src" ] && [ "$out" -nt "$HERE/link-w64.sh" ] && return 0
	cmd=$(make -n -W "$src" "$rel" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $rel")
	[ -n "$cmd" ] || { echo "link-w64: no compile command for $rel" >&2; touch "$fail"; return 0; }
	( eval "${cmd//-c -o $rel/-c -o $out} $defs" >&2 ) || { echo "link-w64: $rel failed" >&2; touch "$fail"; } &
	while [ "$(jobs -r | wc -l)" -ge "$J" ]; do wait -n; done
}
unixobjs() { # DIR LIB: the objects of DIR's LIB.so
	sed -n "/^dlls\/$1\/$2.so:/,/^\t/p" Makefile | grep -o "dlls/$1/[a-z_0-9/-]*\.o" | sort -u
}
for spec in crypt32:crypt32 ws2_32:ws2_32 msv1_0:msv1_0 dnsapi:dnsapi nsiproxy.sys:nsiproxy; do
	dir=${spec%%:*} lib=${spec##*:}
	mkdir -p "gexw64/$lib"
	for c in $(unixobjs "$dir" "$lib"); do
		b=$(basename "$c" .o)
		compile "dlls/$dir/$b.c" "$c" "gexw64/$lib/$b.o" "-D__wine_unix_call_funcs=${lib}__wine_unix_call_funcs \
			-D__wine_unix_call_wow64_funcs=${lib}__wine_unix_call_wow64_funcs"
	done
	reg="$reg $lib.so:data:__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
done
libs=""
if grep -q '^dlls/win32u/win32u.so:' Makefile; then
	# win32u.so: its syscalls register at __wine_unix_lib_init; the names it
	# shares with ntdll.so renamed apart. FreeType through dlopen.
	: "${W64_DEPS:?set W64_DEPS to the install w64-deps.sh made, for win32u and winewayland.drv}"
	# their sources include generated headers (widl's), which a tree that
	# made only the loader has not got
	nice -n 10 make -j"$J" include/all >&2
	defs="-D__wine_unix_lib_init=win32u___wine_unix_lib_init"
	for n in alloc_user_handle destroy_thread_windows free_user_handle get_virtual_screen_rect get_window_thread \
		init_startup_info is_desktop_class is_message_class is_window_visible mirror_region send_notify_message shared_session; do
		defs="$defs -D$n=win32u_$n"
	done
	mkdir -p gexw64/win32u
	for c in $(unixobjs win32u win32u); do
		rel=${c#dlls/win32u/}; compile "dlls/win32u/${rel%.o}.c" "$c" "gexw64/win32u/${rel//\//_}" "$defs"
	done
	reg="$reg win32u.so:func:__wine_unix_lib_init=win32u___wine_unix_lib_init"
	if grep -q 'SONAME_LIBFREETYPE' include/config.h; then
		for f in $(grep -o "LOAD_FUNCPTR(FT_[A-Za-z_]*)" "$WF/dlls/win32u/freetype.c" | sort -u | sed 's/LOAD_FUNCPTR(\(.*\))/\1/'); do
			reg="$reg libfreetype.so.6:func:$f"
		done
		libs="$libs $W64_DEPS/lib/libfreetype.a"
	fi
fi
if grep -q '^dlls/winewayland.drv/winewayland.so:' Makefile; then
	# winewayland.so, the display driver; its protocol sources are generated
	# in the tree first.
	defs="-D__wine_unix_call_funcs=winewayland__wine_unix_call_funcs -Dget_region_data=winewayland_get_region_data"
	mkdir -p gexw64/winewayland
	gen=$(unixobjs winewayland.drv winewayland | sed -n 's/-protocol\.o$/-protocol.c/p')
	# shellcheck disable=SC2086
	nice -n 10 make -j"$J" $gen $(echo "$gen" | sed 's/-protocol\.c$/-client-protocol.h/') >&2
	for c in $(unixobjs winewayland.drv winewayland); do
		b=$(basename "$c" .o)
		if [ -f "$WF/dlls/winewayland.drv/$b.c" ]; then compile "dlls/winewayland.drv/$b.c" "$c" "gexw64/winewayland/$b.o" "$defs"
		else
			cmd=$(make -n -W "dlls/winewayland.drv/$b.c" "$c" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $c")
			objs="$objs gexw64/winewayland/$b.o"
			( eval "${cmd//-c -o $c/-c -o gexw64/winewayland/$b.o} $defs" >&2 ) || { echo "link-w64: $c failed" >&2; touch "$fail"; }
		fi
	done
	reg="$reg winewayland.so:data:__wine_unix_call_funcs=winewayland__wine_unix_call_funcs"
	libs="$libs $W64_DEPS/lib/libwayland-client.a $W64_DEPS/lib/libxkbcommon.a $W64_DEPS/lib/libxkbregistry.a $W64_DEPS/lib/libxml2.a"
fi
wait
[ ! -e "$fail" ] || { echo "link-w64: compile failed; not linking" >&2; exit 1; }
python3 "$HERE/dlreg.py" $reg > gexw64/reg.c
"$HERE/wcc64" -O1 -Wl,--gex-guest-x86-64 -o "$OUT" loader/main.o gexw64/reg.c $objs $libs
