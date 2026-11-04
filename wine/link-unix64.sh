#!/bin/bash
# Link 64-bit Wine's loader: 64-bit native Wine's Unix side (loader/main.o and
# ntdll's Unix objects, LP64 wasm64 lowered to wasm32) as one closed-world
# program, the ELF64 container gexc runs, flagged as running x86-64 Windows
# code. The PE side is Wine's x86_64
# DLLs, translated.
#
#   wine/link-unix64.sh WINE_BUILD OUT   (GEX_WASM64_SYSROOT set)
#
# Also the Unix libraries crypt32, ws2_32, msv1_0, dnsapi and nsiproxy (as
# 32-bit Wine's wine32/link-loader.sh), mountmgr (volumes and their names),
# localspl, winspool and wineps (the spooler and the PostScript driver),
# registered with dlreg.py, secur32 (+ GnuTLS) when
# the tree was configured --with-gnutls, and, when the tree was
# configured with them (wine64-deps.sh's libraries: WINE64_DEPS), win32u
# and dwrite (+ FreeType), winewayland.drv (+ libwayland, xkbcommon) and opengl32
# (+ gex-gl's EGL, guest/gl/gexgl.c compiled LP64); wineoss.drv's
# (audio: OSSv4 against gex-kernel's /dev/dsp, as 32-bit Wine's).
# WINE64_MEDIA=lazy: winegstreamer's too, over FFmpeg (WINE64_DEPS), as
# winegstreamer.so beside OUT, which the loader dlopens when a program first
# plays media.
# WINE64_PADS=lazy: winebus's (game controllers), with SDL2 (WINE64_DEPS) as
# libSDL2-2.0.so.0 beside OUT, which winebus dlopens when it starts (only
# when the page has a controller).
# WINE_BUILD is a wasm64 build tree of the Wine fork (configure
# --host=wasm64-unknown-linux-musl CC=wine/wcc64
# CXX=wine/wcc64++); the objects are made there first.
set -eu
J=${JOBS:-6}
HERE=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 2 ] || { sed -n 2,12p "$0" >&2; exit 2; }
B=$(cd "$1" && pwd) OUT=$2
# WINE64_MEM64=1: the memory64 flavour, for a
# tree configured with CC="wcc64 -mgex-memory64".
M64=; [ "${WINE64_MEM64:-0}" = 0 ] || M64=-mgex-memory64
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (toolchain/sysroot/build64.sh)}"
cd "$B"
objs=$(sed -n '/^dlls\/ntdll\/ntdll.so:/,/^\t/p' Makefile | grep -o 'dlls/ntdll/unix/[a-z_0-9]*\.o' | sort -u |
       grep -v 'signal_arm\|signal_i386\|signal_x86_64\|signal_wasm32')
nice -n 10 make -j"$J" CXX="$HERE/wcc64++${M64:+ $M64}" loader/main.o $objs >&2
mkdir -p gexwine64
reg="ntdll.so:func:__wine_main"
# The Unix libraries without a display or other dependencies, as 32-bit Wine's
# link-loader.sh links them: each one's objects compiled again with its
# __wine_unix_call_funcs renamed apart, and dlreg's registry naming them.
WF=$(sed -n 's/^srcdir *= *//p' Makefile); WF=$(cd "$WF" && pwd)
fail=gexwine64/.fail; rm -f "$fail"
# fresh OUT SRC: OUT is newer than SRC, this script, the headers beside SRC
# and the newest header in include/ (a header change rebuilds the objects
# that include it when the tree is linked again in place).
inc=$(find "$WF/include" -name '*.h' -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2-)
fresh() {
	[ "$1" -nt "$2" ] && [ "$1" -nt "$HERE/link-unix64.sh" ] && [ "$1" -nt "$inc" ] &&
		[ -z "$(find "$(dirname "$2")" -maxdepth 1 -name '*.h' -newer "$1" | head -1)" ]
}
# compile SRC_REL OBJ_REL OUT DEFS: OUT from the build's command for OBJ_REL,
# with DEFS, unless OUT is fresh; at most J at once.
compile() {
	local src=$WF/$1 rel=$2 out=$3 defs=$4 cmd
	[ -f "$src" ] || src=${src%.c}.cpp  # (win32u's syscall_wasm64.cpp)
	objs="$objs $out"
	fresh "$out" "$src" && return 0
	cmd=$(make -n -W "$src" "$rel" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $rel")
	[ -n "$cmd" ] || { echo "link-wine64: no compile command for $rel" >&2; touch "$fail"; return 0; }
	( eval "${cmd//-c -o $rel/-c -o $out} $defs" >&2 ) || { echo "link-wine64: $rel failed" >&2; touch "$fail"; } &
	while [ "$(jobs -r | wc -l)" -ge "$J" ]; do wait -n; done
}
unixobjs() { # DIR LIB: the objects of DIR's LIB.so
	sed -n "/^dlls\/$1\/$2.so:/,/^\t/p" Makefile | grep -o "dlls/$1/[a-z_0-9/-]*\.o" | sort -u
}
tls=""; grep -q 'define SONAME_LIBGNUTLS' include/config.h && tls="secur32:secur32"
for spec in crypt32:crypt32 ws2_32:ws2_32 msv1_0:msv1_0 dnsapi:dnsapi nsiproxy.sys:nsiproxy mountmgr.sys:mountmgr localspl:localspl winspool.drv:winspool wineps.drv:wineps $tls; do
	dir=${spec%%:*} lib=${spec##*:}
	mkdir -p "gexwine64/$lib"
	for c in $(unixobjs "$dir" "$lib"); do
		b=$(basename "$c" .o)
		compile "dlls/$dir/$b.c" "$c" "gexwine64/$lib/$b.o" "-D__wine_unix_call_funcs=${lib}__wine_unix_call_funcs \
			-D__wine_unix_call_wow64_funcs=${lib}__wine_unix_call_wow64_funcs"
	done
	reg="$reg $lib.so:data:__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
done
libs=""
if [ -n "$tls" ]; then
	# GnuTLS (WINE64_DEPS): crypt32 and secur32's schannel dlopen
	# libgnutls.so.30 and look up each function they use.
	: "${WINE64_DEPS:?set WINE64_DEPS to the install wine64-deps.sh made, for GnuTLS}"
	for f in $(grep -ohE 'LOAD_FUNCPTR\( *gnutls_[A-Za-z0-9_]+|dlsym\( *libgnutls_handle, *"gnutls_[A-Za-z0-9_]+"' \
			"$WF/dlls/crypt32/unixlib.c" "$WF/dlls/secur32/schannel_gnutls.c" | sed -E 's/.*[("] *(gnutls_[A-Za-z0-9_]+)"?$/\1/' | sort -u); do
		reg="$reg libgnutls.so.30:func:$f"
	done
	tlslibs="$WINE64_DEPS/lib/libgnutls.a $WINE64_DEPS/lib/libhogweed.a $WINE64_DEPS/lib/libnettle.a"
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
	: "${WINE64_DEPS:?set WINE64_DEPS to the install wine64-deps.sh made, for win32u and winewayland.drv}"
	# their sources include generated headers (widl's), which a tree that
	# made only the loader has not got
	nice -n 10 make -j"$J" include/all >&2
	defs="-D__wine_unix_lib_init=win32u___wine_unix_lib_init"
	for n in alloc_user_handle destroy_thread_windows free_user_handle get_virtual_screen_rect get_window_thread \
		init_startup_info is_desktop_class is_message_class is_window_visible mirror_region send_notify_message shared_session; do
		defs="$defs -D$n=win32u_$n"
	done
	mkdir -p gexwine64/win32u
	for c in $(unixobjs win32u win32u); do
		rel=${c#dlls/win32u/}; compile "dlls/win32u/${rel%.o}.c" "$c" "gexwine64/win32u/${rel//\//_}" "$defs"
	done
	reg="$reg win32u.so:func:__wine_unix_lib_init=win32u___wine_unix_lib_init"
	if grep -q 'SONAME_LIBFREETYPE' include/config.h; then
		# dwrite.so: DirectWrite's glyph metrics, outlines and bitmaps, FreeType
		# through dlopen as win32u's. Without it every glyph run has empty
		# bounds, and Direct2D and DirectWrite draw no text at all.
		mkdir -p gexwine64/dwrite
		for c in $(unixobjs dwrite dwrite); do
			b=$(basename "$c" .o)
			compile "dlls/dwrite/$b.c" "$c" "gexwine64/dwrite/$b.o" "-D__wine_unix_call_funcs=dwrite__wine_unix_call_funcs \
				-D__wine_unix_call_wow64_funcs=dwrite__wine_unix_call_wow64_funcs"
		done
		reg="$reg dwrite.so:data:__wine_unix_call_funcs=dwrite__wine_unix_call_funcs"
		for f in $(grep -oh "LOAD_FUNCPTR(FT_[A-Za-z_]*)" "$WF/dlls/win32u/freetype.c" "$WF/dlls/dwrite/freetype.c" | sort -u |
				sed 's/LOAD_FUNCPTR(\(.*\))/\1/'); do
			reg="$reg libfreetype.so.6:func:$f"
		done
		libs="$libs $WINE64_DEPS/lib/libfreetype.a"
	fi
fi
if grep -q '^dlls/winewayland.drv/winewayland.so:' Makefile; then
	# winewayland.so, the display driver; its protocol sources are generated
	# in the tree first.
	defs="-D__wine_unix_call_funcs=winewayland__wine_unix_call_funcs -D__wine_unix_call_wow64_funcs=winewayland__wine_unix_call_wow64_funcs -Dget_region_data=winewayland_get_region_data"
	mkdir -p gexwine64/winewayland
	gen=$(unixobjs winewayland.drv winewayland | sed -n 's/-protocol\.o$/-protocol.c/p')
	# shellcheck disable=SC2086
	nice -n 10 make -j"$J" $gen $(echo "$gen" | sed 's/-protocol\.c$/-client-protocol.h/') >&2
	for c in $(unixobjs winewayland.drv winewayland); do
		b=$(basename "$c" .o)
		if [ -f "$WF/dlls/winewayland.drv/$b.c" ]; then compile "dlls/winewayland.drv/$b.c" "$c" "gexwine64/winewayland/$b.o" "$defs"
		else
			cmd=$(make -n -W "dlls/winewayland.drv/$b.c" "$c" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $c")
			objs="$objs gexwine64/winewayland/$b.o"
			( eval "${cmd//-c -o $c/-c -o gexwine64/winewayland/$b.o} $defs" >&2 ) || { echo "link-wine64: $c failed" >&2; touch "$fail"; }
		fi
	done
	reg="$reg winewayland.so:data:__wine_unix_call_funcs=winewayland__wine_unix_call_funcs"
	libs="$libs $WINE64_DEPS/lib/libwayland-egl.a $WINE64_DEPS/lib/libwayland-client.a $WINE64_DEPS/lib/libxkbcommon.a $WINE64_DEPS/lib/libxkbregistry.a $WINE64_DEPS/lib/libxml2.a"
fi
if grep -q '^dlls/opengl32/opengl32.so:' Makefile && grep -q 'SONAME_LIBEGL' include/config.h; then
	# opengl32.so; win32u's EGL is gex-gl's (guest/gl/gexgl.c, LP64), found
	# by dlopen("libEGL.so.1"), as 32-bit Wine's link-loader.sh links it. gex-gl
	# finds libwayland-client the same way (RTLD_NOLOAD).
	mkdir -p gexwine64/opengl32
	for c in $(unixobjs opengl32 opengl32); do
		b=$(basename "$c" .o)
		compile "dlls/opengl32/$b.c" "$c" "gexwine64/opengl32/$b.o" "-D__wine_unix_call_funcs=opengl32__wine_unix_call_funcs -D__wine_unix_call_wow64_funcs=opengl32__wine_unix_call_wow64_funcs"
	done
	reg="$reg opengl32.so:data:__wine_unix_call_funcs=opengl32__wine_unix_call_funcs"
	gl=$HERE/../guest/gl
	if ! [ gexwine64/gexgl.o -nt "$gl/gexgl.c" ] || [ -n "$(find "$gl" -maxdepth 1 -newer gexwine64/gexgl.o \( -name '*.h' -o -name '*.inc' \))" ]; then
		nice -n 10 "$HERE/wcc64" $M64 -O2 -c -o gexwine64/gexgl.o "$gl/gexgl.c" >&2 || { echo "link-wine64: gexgl.c failed" >&2; touch "$fail"; }
	fi
	objs="$objs gexwine64/gexgl.o"
	for f in $(llvm-nm --defined-only --extern-only gexwine64/gexgl.o | awk '$2 == "T" { print $3 }'); do reg="$reg libEGL.so.1:func:$f"; done
	for f in wl_display_flush wl_proxy_destroy wl_proxy_get_display wl_proxy_get_id wl_proxy_marshal wl_proxy_marshal_constructor; do
		reg="$reg libwayland-client.so.0:func:$f"
	done
	reg="$reg libwayland-client.so.0:data:wl_callback_interface"
fi
# wineoss.so (audio), a Unix library only: mmdevapi loads it by name
# (__wine_load_unix_lib), no PE side. Configure leaves it out
# (--without-oss: no OSSv4 header in the sysroot), so as 32-bit Wine's
# wine32/link-loader.sh, it is compiled with crypt32's unixlib command, against
# wine32/oss's sys/soundcard.h (OSSv4's API, which gex-kernel's /dev/dsp and
# /dev/mixer answer). Without it an x64 game's audio device fails to open
# (mmdevapi: "No driver from pulse,alsa,oss,coreaudio could be
# initialized"; OpenAL Soft: "Error opening OpenAL device").
tmpl=$(make -n -W "$WF/dlls/crypt32/unixlib.c" dlls/crypt32/unixlib.o 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' |
	grep -- "-c -o dlls/crypt32/unixlib.o")
[ -n "$tmpl" ] || { echo "link-wine64: no compile command for crypt32's unixlib (wineoss's template)" >&2; touch "$fail"; }
# And gexasio.so, the Unix side of gex audio (ASIO) (dlls/gexasio: an ASIO
# driver over the same /dev/dsp), the same way, when the tree has it.
uxl="wineoss.drv:wineoss:midipatch,oss,ossmidi"
[ -f "$WF/dlls/gexasio/unixlib.c" ] && uxl="$uxl gexasio:gexasio:unixlib"
for spec in $uxl; do
	dir=${spec%%:*} rest=${spec#*:}; lib=${rest%%:*}; srcs=${rest#*:}
	mkdir -p "gexwine64/$lib"
	for b in ${srcs//,/ }; do
		out=gexwine64/$lib/$b.o; objs="$objs $out"
		fresh "$out" "$WF/dlls/$dir/$b.c" && continue
		cmd=${tmpl//-c -o dlls\/crypt32\/unixlib.o/-c -o $out}; cmd=${cmd//$WF\/dlls\/crypt32\/unixlib.c/$WF\/dlls\/$dir\/$b.c}
		cmd=${cmd//-I$WF\/dlls\/crypt32/-I$WF\/dlls\/$dir -I$HERE\/oss}; cmd=${cmd//-Idlls\/crypt32/}; cmd=${cmd//-D_CRYPT32_/}
		( eval "$cmd -D__wine_unix_call_funcs=${lib}__wine_unix_call_funcs -D__wine_unix_call_wow64_funcs=${lib}__wine_unix_call_wow64_funcs" >&2 ) ||
			{ echo "link-wine64: $dir/$b.c failed" >&2; touch "$fail"; } &
		while [ "$(jobs -r | wc -l)" -ge "$J" ]; do wait -n; done
	done
	reg="$reg $lib.so:data:__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
done
# WINE64_MEDIA=lazy: winegstreamer.so, the Unix side of winegstreamer (Media
# Foundation's and DirectShow's decoders and media source, the WM reader),
# over FFmpeg (WINE64_DEPS's: wine64-deps.sh) rather than GStreamer: the
# tree's wg_*_ffmpeg.c and unixlib_ffmpeg.c, in place of its GStreamer files,
# with the same template. Not linked in: a side module beside the loader
# (OUT's directory), which ntdll dlopens on the first wg_* call; what it
# imports (libc's functions, ntdll's debug channels) the loader exports
# (-Wl,--export), and nothing more of it is in the loader. The PE side is
# unchanged. Unset, 0 or off leaves it out (wine64-dist.sh passes lazy by
# default).
media=
case ${WINE64_MEDIA:-0} in
0|off) ;;
lazy) [ -f "$WF/dlls/winegstreamer/unixlib_ffmpeg.c" ] || { echo "link-wine64: WINE64_MEDIA=lazy, and $WF has no dlls/winegstreamer/unixlib_ffmpeg.c" >&2; exit 1; }
	media=1 ;;
*) echo "link-wine64: WINE64_MEDIA=$WINE64_MEDIA (lazy, or 0 or off)" >&2; exit 1 ;;
esac
if [ -n "$media" ]; then
	: "${WINE64_DEPS:?set WINE64_DEPS to the install wine64-deps.sh made, for FFmpeg}"
	[ -f "$WINE64_DEPS/lib/libavcodec.a" ] || { echo "link-wine64: no FFmpeg in $WINE64_DEPS (wine64-deps.sh)" >&2; exit 1; }
	# its headers include widl's (mfobjects.h and the rest)
	nice -n 10 make -j"$J" include/all >&2
	mkdir -p gexwine64/winegstreamer
	wgobjs=
	for b in unixlib_ffmpeg wg_format_ffmpeg wg_parser_ffmpeg wg_transform_ffmpeg; do
		out=gexwine64/winegstreamer/$b.o; wgobjs="$wgobjs $out"
		fresh "$out" "$WF/dlls/winegstreamer/$b.c" && continue
		cmd=${tmpl//-c -o dlls\/crypt32\/unixlib.o/-c -o $out}; cmd=${cmd//$WF\/dlls\/crypt32\/unixlib.c/$WF\/dlls\/winegstreamer\/$b.c}
		cmd=${cmd//-I$WF\/dlls\/crypt32/-I$WF\/dlls\/winegstreamer -I$WINE64_DEPS\/include}; cmd=${cmd//-Idlls\/crypt32/}; cmd=${cmd//-D_CRYPT32_/}
		( eval "$cmd" >&2 ) || { echo "link-wine64: winegstreamer/$b.c failed" >&2; touch "$fail"; } &
		while [ "$(jobs -r | wc -l)" -ge "$J" ]; do wait -n; done
	done
fi
# WINE64_PADS=lazy: winebus.so, the Unix side of winebus (the HID bus game
# controllers come through), in the loader as the libraries above; its
# SDL bus (the tree configured with SDL: wine64-dist.sh) dlopens
# libSDL2-2.0.so.0, linked here from WINE64_DEPS's SDL2 as a side module
# beside OUT (what winebus asks of it, the functions bus_sdl.c loads), as
# winegstreamer.so is. A static program's dlopen finds a bare soname beside
# the program (gex-dylink). Unset, 0 or off leaves both out.
pads=
case ${WINE64_PADS:-0} in
0|off) ;;
lazy) grep -q 'define SONAME_LIBSDL2' include/config.h || { echo "link-wine64: WINE64_PADS=lazy, and the tree was configured without SDL" >&2; exit 1; }
	pads=1 ;;
*) echo "link-wine64: WINE64_PADS=$WINE64_PADS (lazy, or 0 or off)" >&2; exit 1 ;;
esac
if [ -n "$pads" ]; then
	: "${WINE64_DEPS:?set WINE64_DEPS to the install wine64-deps.sh made, for SDL2}"
	[ -f "$WINE64_DEPS/lib/libSDL2.a" ] || { echo "link-wine64: no SDL2 in $WINE64_DEPS (wine64-deps.sh)" >&2; exit 1; }
	mkdir -p gexwine64/winebus
	for c in $(unixobjs winebus.sys winebus); do
		b=$(basename "$c" .o)
		compile "dlls/winebus.sys/$b.c" "$c" "gexwine64/winebus/$b.o" "-D__wine_unix_call_funcs=winebus__wine_unix_call_funcs \
			-D__wine_unix_call_wow64_funcs=winebus__wine_unix_call_wow64_funcs"
	done
	reg="$reg winebus.so:data:__wine_unix_call_funcs=winebus__wine_unix_call_funcs"
fi
# WINE64_DIRK=1: d3d11.so, the Unix side of the D3D shim (dxvk's src/dirk/pe;
# Mesa pins dxvk, and ports/mesa/build.sh with GEX_MESA_DIRK=1 fetches it
# into third_party/mesa/subprojects/dxvk), with the same template. Its
# d3d11.dll is in the native core only with WINE64_EXTRA_DLLS, a page layer
# apps turn on with d3d=1.
dk=$HERE/../third_party/mesa/subprojects/dxvk/src/dirk/pe
if [ "${WINE64_DIRK:-0}" = 1 ]; then
	[ -f "$dk/unixlib.c" ] || { echo "link-wine64: WINE64_DIRK=1 needs third_party/mesa/subprojects/dxvk (cd third_party/mesa && meson subprojects download dxvk)" >&2; exit 1; }
	mkdir -p gexw64/dirk
	out=gexw64/dirk/unixlib.o; objs="$objs $out"
	if ! [ "$out" -nt "$dk/unixlib.c" ] || ! [ "$out" -nt "$dk/unixlib.h" ] || ! [ "$out" -nt "$HERE/link-unix64.sh" ]; then
		cmd=${tmpl//-c -o dlls\/crypt32\/unixlib.o/-c -o $out}; cmd=${cmd//$WF\/dlls\/crypt32\/unixlib.c/$dk\/unixlib.c}
		cmd=${cmd//-I$WF\/dlls\/crypt32/-I$dk}; cmd=${cmd//-Idlls\/crypt32/}; cmd=${cmd//-D_CRYPT32_/}
		( eval "$cmd -D__wine_unix_call_funcs=d3d11__wine_unix_call_funcs" >&2 ) ||
			{ echo "link-wine64: dirk's unixlib.c failed" >&2; touch "$fail"; }
	fi
	reg="$reg d3d11.so:data:__wine_unix_call_funcs=d3d11__wine_unix_call_funcs"
fi
wait
[ ! -e "$fail" ] || { echo "link-wine64: compile failed; not linking" >&2; exit 1; }
if [ -n "$M64" ]; then
	# the sysroot's libgex.a is the lowered flavour's: its dlopen again, with
	# 64-bit addresses at its imports (then nothing of it comes from there)
	"$HERE/wcc64" $M64 -O2 -c -o gexwine64/dl64.o "$HERE/../toolchain/sysroot/libgex/dl.c" || exit 1
	objs="$objs gexwine64/dl64.o"
fi
# WINE64_WOW64=1: a WoW64 process's 32-bit DLLs call each library's
# __wine_unix_call_wow64_funcs, which ntdll asks dlopen's registry for (as
# for __wine_unix_call_funcs): registered for every library whose objects
# define one.
if [ "${WINE64_WOW64:-0}" != 0 ]; then
	wow=$(llvm-nm --defined-only --extern-only $objs 2>/dev/null | awk '$3 ~ /__wine_unix_call_wow64_funcs$/ { print $3 }' | sort -u)
	for r in $reg; do
		case $r in
		*:data:__wine_unix_call_funcs=*)
			w=${r##*=}; w=${w%__wine_unix_call_funcs}__wine_unix_call_wow64_funcs
			if grep -qx "$w" <<< "$wow"; then reg="$reg ${r%%:*}:data:__wine_unix_call_wow64_funcs=$w"
			else echo "link-wine64: ${r%%:*} has no __wine_unix_call_wow64_funcs (no WoW64 calls)" >&2; fi ;;
		esac
	done
fi
python3 "$HERE/dlreg.py" $reg > gexwine64/reg.c
# The side modules, and what of the loader each imports: the loader exports
# it (from an archive when nothing else of the loader uses it: libm's).
sides=
if [ -n "$media" ]; then
	so=$(dirname "$OUT")/winegstreamer.so
	"$HERE/wcc64" $M64 -O1 -shared -Wl,--unresolved-symbols=import-dynamic -o "$so" $wgobjs \
		"$WINE64_DEPS/lib/libavformat.a" "$WINE64_DEPS/lib/libavcodec.a" "$WINE64_DEPS/lib/libswscale.a" \
		"$WINE64_DEPS/lib/libswresample.a" "$WINE64_DEPS/lib/libavutil.a" || exit 1
	rm -f "$so.64.wasm"
	sides="$sides $so"
fi
if [ -n "$pads" ]; then
	so=$(dirname "$OUT")/libSDL2-2.0.so.0
	# shellcheck disable=SC2046
	"$HERE/wcc64" $M64 -O1 -shared -Wl,--unresolved-symbols=import-dynamic -o "$so" \
		$(grep -ohE 'LOAD_FUNCPTR\(SDL_[A-Za-z0-9_]+\)|dlsym\(sdl_handle, "SDL_[A-Za-z0-9_]+"' "$WF/dlls/winebus.sys/bus_sdl.c" |
			grep -oE 'SDL_[A-Za-z0-9_]+' | sort -u | sed 's/^/-Wl,-u,/') "$WINE64_DEPS/lib/libSDL2.a" || exit 1
	rm -f "$so.64.wasm"
	sides="$sides $so"
fi
exports=
for so in $sides; do
	exports="$exports $(python3 "$HERE/side-imports.py" "$so")" || exit 1
done
[ -z "$sides" ] || exports=$(printf '%s\n' $exports | sort -u | sed 's/^/-Wl,--export=/')
"$HERE/wcc64" $M64 -O1 -Wl,--gex-guest-x86-64 -o "$OUT" loader/main.o gexwine64/reg.c $objs $libs $exports
