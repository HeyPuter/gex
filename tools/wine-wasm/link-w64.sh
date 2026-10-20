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
# W3's w3/link-loader.sh), registered with dlreg.py, secur32 (+ GnuTLS) when
# the tree was configured --with-gnutls, and, when the tree was
# configured with them (W64.4, w64-deps.sh's libraries: W64_DEPS), win32u
# (+ FreeType), winewayland.drv (+ libwayland, xkbcommon) and opengl32
# (+ gex-gl's EGL, guest/gl/gexgl.c compiled LP64); and wineoss.drv's
# (audio: OSSv4 against gex-kernel's /dev/dsp, as W3's).
# WINE_BUILD is a wasm64 build tree of the Wine fork (configure
# --host=wasm64-unknown-linux-musl CC=tools/wine-wasm/wcc64
# CXX=tools/wine-wasm/wcc64++); the objects are made there first.
set -eu
J=${JOBS:-6}
HERE=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 2 ] || { sed -n 2,12p "$0" >&2; exit 2; }
B=$(cd "$1" && pwd) OUT=$2
# W64_MEM64=1: the memory64 flavour (docs/wine-native-x64.md §0.1), for a
# tree configured with CC="wcc64 -mgex-memory64".
M64=; [ "${W64_MEM64:-0}" = 0 ] || M64=-mgex-memory64
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
cd "$B"
objs=$(sed -n '/^dlls\/ntdll\/ntdll.so:/,/^\t/p' Makefile | grep -o 'dlls/ntdll/unix/[a-z_0-9]*\.o' | sort -u |
       grep -v 'signal_arm\|signal_i386\|signal_x86_64\|signal_wasm32')
nice -n 10 make -j"$J" CXX="$HERE/wcc64++${M64:+ $M64}" loader/main.o $objs >&2
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
tls=""; grep -q 'define SONAME_LIBGNUTLS' include/config.h && tls="secur32:secur32"
for spec in crypt32:crypt32 ws2_32:ws2_32 msv1_0:msv1_0 dnsapi:dnsapi nsiproxy.sys:nsiproxy $tls; do
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
if [ -n "$tls" ]; then
	# GnuTLS (W64_DEPS): crypt32 and secur32's schannel dlopen
	# libgnutls.so.30 and look up each function they use.
	: "${W64_DEPS:?set W64_DEPS to the install w64-deps.sh made, for GnuTLS}"
	for f in $(grep -ohE 'LOAD_FUNCPTR\( *gnutls_[A-Za-z0-9_]+|dlsym\( *libgnutls_handle, *"gnutls_[A-Za-z0-9_]+"' \
			"$WF/dlls/crypt32/unixlib.c" "$WF/dlls/secur32/schannel_gnutls.c" | sed -E 's/.*[("] *(gnutls_[A-Za-z0-9_]+)"?$/\1/' | sort -u); do
		reg="$reg libgnutls.so.30:func:$f"
	done
	tlslibs="$W64_DEPS/lib/libgnutls.a $W64_DEPS/lib/libhogweed.a $W64_DEPS/lib/libnettle.a"
	# gnulib's threadlib takes thrd_exit and the pthread calls weakly (to
	# see whether threads are linked in), which pulls nothing from libc.a:
	# left undefined, each is an import (GOT.func) the loader is never
	# given. Every weak name libc.a defines is pulled in.
	# shellcheck disable=SC2086
	for f in $(comm -12 <(llvm-nm $tlslibs 2>/dev/null | awk '$1 == "w" { print $2 }' | sort -u) \
			<(llvm-nm --defined-only "$GEX_WASM64_SYSROOT/lib/libc.a" 2>/dev/null | awk '$2 ~ /^[TW]$/ { print $3 }' | sort -u)); do
		libs="$libs -Wl,-u,$f"
	done
	libs="$libs $tlslibs"
fi
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
	defs="-D__wine_unix_call_funcs=winewayland__wine_unix_call_funcs -D__wine_unix_call_wow64_funcs=winewayland__wine_unix_call_wow64_funcs -Dget_region_data=winewayland_get_region_data"
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
	libs="$libs $W64_DEPS/lib/libwayland-egl.a $W64_DEPS/lib/libwayland-client.a $W64_DEPS/lib/libxkbcommon.a $W64_DEPS/lib/libxkbregistry.a $W64_DEPS/lib/libxml2.a"
fi
if grep -q '^dlls/opengl32/opengl32.so:' Makefile && grep -q 'SONAME_LIBEGL' include/config.h; then
	# opengl32.so; win32u's EGL is gex-gl's (guest/gl/gexgl.c, LP64), found
	# by dlopen("libEGL.so.1"), as W3's link-loader.sh links it. gex-gl
	# finds libwayland-client the same way (RTLD_NOLOAD).
	mkdir -p gexw64/opengl32
	for c in $(unixobjs opengl32 opengl32); do
		b=$(basename "$c" .o)
		compile "dlls/opengl32/$b.c" "$c" "gexw64/opengl32/$b.o" "-D__wine_unix_call_funcs=opengl32__wine_unix_call_funcs -D__wine_unix_call_wow64_funcs=opengl32__wine_unix_call_wow64_funcs"
	done
	reg="$reg opengl32.so:data:__wine_unix_call_funcs=opengl32__wine_unix_call_funcs"
	gl=$HERE/../../guest/gl
	if ! [ gexw64/gexgl.o -nt "$gl/gexgl.c" ] || [ -n "$(find "$gl" -maxdepth 1 -newer gexw64/gexgl.o \( -name '*.h' -o -name '*.inc' \))" ]; then
		nice -n 10 "$HERE/wcc64" $M64 -O2 -c -o gexw64/gexgl.o "$gl/gexgl.c" >&2 || { echo "link-w64: gexgl.c failed" >&2; touch "$fail"; }
	fi
	objs="$objs gexw64/gexgl.o"
	for f in $(llvm-nm --defined-only --extern-only gexw64/gexgl.o | awk '$2 == "T" { print $3 }'); do reg="$reg libEGL.so.1:func:$f"; done
	for f in wl_display_flush wl_proxy_destroy wl_proxy_get_display wl_proxy_get_id wl_proxy_marshal wl_proxy_marshal_constructor; do
		reg="$reg libwayland-client.so.0:func:$f"
	done
	reg="$reg libwayland-client.so.0:data:wl_callback_interface"
fi
# wineoss.so (audio), a Unix library only: mmdevapi loads it by name
# (__wine_load_unix_lib), no PE side. Configure leaves it out
# (--without-oss: no OSSv4 header in the sysroot), so as W3's
# w3/link-loader.sh, it is compiled with crypt32's unixlib command, against
# w3/oss's sys/soundcard.h (OSSv4's API, which gex-kernel's /dev/dsp and
# /dev/mixer answer). Without it an x64 game's audio device fails to open
# (mmdevapi: "No driver from pulse,alsa,oss,coreaudio could be
# initialized"; OpenAL Soft: "Error opening OpenAL device").
tmpl=$(make -n -W "$WF/dlls/crypt32/unixlib.c" dlls/crypt32/unixlib.o 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' |
	grep -- "-c -o dlls/crypt32/unixlib.o")
[ -n "$tmpl" ] || { echo "link-w64: no compile command for crypt32's unixlib (wineoss's template)" >&2; touch "$fail"; }
# And gexasio.so, the Unix side of gex audio (ASIO) (dlls/gexasio: an ASIO
# driver over the same /dev/dsp), the same way, when the tree has it.
uxl="wineoss.drv:wineoss:midipatch,oss,ossmidi"
[ -f "$WF/dlls/gexasio/unixlib.c" ] && uxl="$uxl gexasio:gexasio:unixlib"
for spec in $uxl; do
	dir=${spec%%:*} rest=${spec#*:}; lib=${rest%%:*}; srcs=${rest#*:}
	mkdir -p "gexw64/$lib"
	for b in ${srcs//,/ }; do
		out=gexw64/$lib/$b.o; objs="$objs $out"
		[ "$out" -nt "$WF/dlls/$dir/$b.c" ] && [ "$out" -nt "$HERE/link-w64.sh" ] && continue
		cmd=${tmpl//-c -o dlls\/crypt32\/unixlib.o/-c -o $out}; cmd=${cmd//$WF\/dlls\/crypt32\/unixlib.c/$WF\/dlls\/$dir\/$b.c}
		cmd=${cmd//-I$WF\/dlls\/crypt32/-I$WF\/dlls\/$dir -I$HERE\/w3\/oss}; cmd=${cmd//-Idlls\/crypt32/}; cmd=${cmd//-D_CRYPT32_/}
		( eval "$cmd -D__wine_unix_call_funcs=${lib}__wine_unix_call_funcs -D__wine_unix_call_wow64_funcs=${lib}__wine_unix_call_wow64_funcs" >&2 ) ||
			{ echo "link-w64: $dir/$b.c failed" >&2; touch "$fail"; } &
		while [ "$(jobs -r | wc -l)" -ge "$J" ]; do wait -n; done
	done
	reg="$reg $lib.so:data:__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
done
wait
[ ! -e "$fail" ] || { echo "link-w64: compile failed; not linking" >&2; exit 1; }
if [ -n "$M64" ]; then
	# the sysroot's libgex.a is the lowered flavour's: its dlopen again, with
	# 64-bit addresses at its imports (then nothing of it comes from there)
	"$HERE/wcc64" $M64 -O2 -c -o gexw64/dl64.o "$HERE/libgex/dl.c" || exit 1
	objs="$objs gexw64/dl64.o"
fi
python3 "$HERE/dlreg.py" $reg > gexw64/reg.c
"$HERE/wcc64" $M64 -O1 -Wl,--gex-guest-x86-64 -o "$OUT" loader/main.o gexw64/reg.c $objs $libs
