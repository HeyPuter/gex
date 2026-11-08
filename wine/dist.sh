#!/bin/bash
# A 64-bit Wine install and a win64 prefix for it,
# built from a Wine tree with the 64-bit Wine work (wine/wine.pins names the
# commit; another is built too, with a note):
#
#   wine/dist.sh WINE CACHE
#
# prints WINE64_DIST=..., WINE64_PREFIX=..., and WINE64_TOOLS=... and WINE64_SRC=...
# (the native tools build and the Wine tree: the native PE side's case),
# WINE64_BUILD=... (the wasm64 build tree: its import libraries, headers and
# libgexrt, which gexcc-win links against),
# for a test, WINE64_NTDLL=... (the native PE ntdll) and
# WINE64_NATIVE=... (the native PE core).
# Each step is cached in CACHE by what it depends on, and skipped when done:
#   tools-TP    Wine's build tools, native (widl and winebuild know wasm64);
#               TP: tools/, include/, libs/, nls/ and the configure inputs
#   pe64-P      the x86_64 PE DLLs, MinGW, and native64 their install, made
#               in pe64-work-* incrementally (with
#               a native x86_64 Unix side, used only to make the prefix)
#   wwl-P       with WINE64_WOW64=1, the i386 and x86_64 PE winewayland.drv
#               (a tree of its own: the display driver's PE halves)
#   wgs-P       winegstreamer's PE halves, likewise (WINE64_MEDIA=lazy, the default)
#   pfx64-P-gex a win64 prefix (wineboot -i, native, no display, as user gex)
#   pfs64-P-gex-R it with 32-bit Wine's session applied (wine32-session.reg; R: that and
#               reg-apply.py): WINE64_PREFIX; each keeps the keys of the
#               installs it is printed with (.gex-dists, wine/prefix-key.sh)
#   deps64-D    the Unix side's display libraries, LP64 wasm64 (wine64-deps.sh:
#               FreeType, libwayland, libxml2, xkbcommon, EGL headers;
#               GnuTLS over nettle and Mozilla's CA bundle, as 32-bit Wine's;
#               FFmpeg; SDL2)
#   b64-K-S     the wasm64 build tree of the Unix side (wcc64), with them:
#               win32u (+ FreeType), winewayland.drv and opengl32 (+ gex-gl's
#               EGL, guest/gl/gexgl.c), secur32 (+ GnuTLS) and
#               wineoss.drv (audio, against gex-kernel's /dev/dsp, as 32-bit Wine's)
#               and winebus in the loader, and winegstreamer.so (+ FFmpeg) and libSDL2-2.0.so.0
#               beside it (WINE64_MEDIA=off, WINE64_PADS=off: not);
#               WINE64_TLS=0 leaves GnuTLS out (S gets -notls)
#   s32-TP-SC   this Wine tree configured for wasm32, not made (build32.sh
#               configure): the compile commands the server is built with;
#               SC: the 32-bit sysroot and libraries, build32.sh and wcc
#   srv-K-V     the wasm32 wineserver (link-wineserver.sh over s32-TP-SC); V:
#               that script and SC; its objects in obj/ (test/page/snapshot
#               relinks them): WINE64_SERVER
#   fon-F       Wine's raster fonts (wine-fon.sh: sfnt2fon over a native
#               FreeType); F: fonts/, tools/sfnt2fon, the script and FreeType's pin
#   dist64-K-S-fF-sV-twinM  with WINE64_TWIN=1, that install plus the memory64
#               loader as x86_64-unix/wine.m64 (the WINE64_MEM64=1 build's)
#               and that build's modules in x86_64-windows, each as
#               NAME.wasm.m64 (M: its core's key)
#   dist64-K-S-fF-sV  the install: x86_64-unix/{wine,wineserver,winegstreamer.so,libSDL2-2.0.so.0 and their gex-rest-only}, x86_64-windows,
#               (with WINE64_WOW64=1 i386-windows and its winewayland.drv:
#               the name gets -wwl; less Win16 and VxD: -no16; less icu: -noicu; ICU data cut to WINE_ICU_LOCALES: -icuL; winegstreamer over FFmpeg: -wgf, less it: -nogst; game controllers: -sdl)
#               share/wine (with the .fon fonts), and etc/ssl/certs/ca-certificates.crt (crypt32's
#               root store; natively the host's /etc is the guest's)
#   ntdll64-K-S the native PE ntdll , made in b64-K-S
#   pen64-K-N   the native PE side: WINE64_NATIVE_DLLS (default ntdll,
#               kernelbase, kernel32, msvcrt, ucrtbase, winewayland.drv (the
#               display driver's PE side), WoW64's DLLs, wineboot, services
#               and explorer) made in b64-K-S and linked as one module behind
#               their skeletons, and every other module that tree builds as
#               one of its own (stage-native64.sh)
#   pen64-K-N-twinM  with WINE64_TWIN=1, that core plus the memory64 build's
#               modules (M: its core's key), each as NAME.wasm.m64
#               (WINE64_NATIVE then names it)
#   b32-K-B     with WINE64_WOW64=1, the wasm32 build tree of the i386 PE side
#               (wine32/wine-build.sh wasm; B: that script, 32-bit Wine's
#               sysroot and libraries): WINE64_BUILD32, the sysroot WINE64_SYSROOT32
#   pen32-K-N32 its native modules: WINE64_NATIVE_DLLS32 as the core, every
#               other one of its own (stage-native64.sh --i386): WINE64_NATIVE32
#   dist64-...-nN[-N32]-sX  the install with the native PE side in place
#               of the PE builtins, both arches, its modules
#               stripped for release (X: stripnames.py; -dbg in place of -sX
#               with WINE64_STRIP=0, names kept): WINE64_DIST
#               (WINE64_NATIVE_PE=0: the install above, translated)
# K is the Wine commit, P the Wine tree less its Unix side (tools, pe64,
# pfx64), D the sysroot, the compiler and the libraries' pins and build,
# S those, gex's link tools, gex-gl's guest library and the OSSv4 header
# (wine32/oss), N those, the PE
# linker and the DLL list.
# Env: GEX_WASM64_SYSROOT (toolchain/sysroot/build64.sh); JOBS (default 16); the
# libraries' tarballs are kept in CACHE/dl, taken from WINE32_DL_SEED and
# 32-bit Wine's downloads (WINE32_CACHE, default ~/.cache/gex/wine32) when there.
# The server, a wasm32 program, is compiled with this tree's own wasm32
# configure (s32 above), over the 32-bit sysroot and libraries: the caller's
# GEX_WASM_SYSROOT and WINE32_DEPS, or, unset, wine's own (`make paths`,
# under WINE32_CACHE), made first when missing (`make sysroot deps`) --
# checked before anything else is built. WoW64's i386 side is built over
# the same two. Needs x86_64-w64-mingw32-gcc. The
# first run builds Wine twice (native and MinGW, then wasm64): about 40
# minutes on 16 threads.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
W=$(cd "${1:?WINE}" && pwd); mkdir -p "${2:?CACHE}"; C=$(cd "$2" && pwd)
# temporary files (stage-native.sh's) under the cache unless TMPDIR says
export TMPDIR=${TMPDIR:-$C/tmp}; mkdir -p "$TMPDIR"
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (toolchain/sysroot/build64.sh)}"
J=${JOBS:-16}
[ -z "$(git -C "$W" status --porcelain --untracked-files=no)" ] || { echo "wine64-dist: $W has uncommitted changes" >&2; exit 1; }
K=$(git -C "$W" rev-parse --short=12 HEAD)
pin=$(sed -n 's/^WINE64_COMMIT *:= *//p' "$here/wine.pins")
[ "$(git -C "$W" rev-parse HEAD)" = "$pin" ] || echo "wine64-dist: $W is at $K, not wine.pins' $(printf %.12s "$pin")" >&2
log() { echo "wine64-dist: $*" >&2; }
# The 32-bit prerequisites first, so a missing one fails now, not after the
# Wine builds: the sysroot and the libraries (no 32-bit Wine tree, and
# nothing of legacy32/ or its wine32.pins is read). The pins they are made
# from: toolchain/pins' MUSL_COMMIT (the sysroot), and libs.pins' FREETYPE_*,
# WAYLAND_*, XKBCOMMON_*, LIBXML2_*, MESON_*, NETTLE_*, GNUTLS_* and CACERT_*
# (deps32.sh).
p32=$(make -s -C "$here" paths) || { log "make -C wine paths failed"; exit 1; }
SR32=${GEX_WASM_SYSROOT:-$(sed -n 's/^GEX_WASM_SYSROOT=//p' <<< "$p32")}
DEPS32=${WINE32_DEPS:-$(sed -n 's/^WINE32_DEPS=//p' <<< "$p32")}
if [ -z "${GEX_WASM_SYSROOT:-}${WINE32_DEPS:-}" ] && ! { [ -d "$SR32" ] && [ -e "$DEPS32/.done" ]; }; then
	log "no 32-bit sysroot or libraries: making them (make -C wine sysroot deps, log $C/wine32-deps.log)"
	nice -n 10 make -C "$here" sysroot deps WINE32_JOBS="$J" > "$C/wine32-deps.log" 2>&1 ||
		{ log "the 32-bit sysroot or libraries failed: $C/wine32-deps.log"; tail -5 "$C/wine32-deps.log" >&2; exit 1; }
fi
[ -d "$SR32" ] && [ -d "$DEPS32/install" ] || { log "no 32-bit sysroot or libraries ($SR32, $DEPS32): make -C wine sysroot deps"; exit 1; }
SC=$( { echo "$SR32 $DEPS32"; cat "$here"/build32.sh "$here"/wcc; } | sha256sum | cut -c1-10)
V=$( { echo "$SC"; cat "$here/link-wineserver.sh"; } | sha256sum | cut -c1-6)
# The tools, the PE DLLs and the prefix do not depend on the Unix side
# (dlls/*/unix, server, loader): keyed by the rest of the tree, so a
# Unix-side change rebuilds only the wasm64 tree and the server.
P=$(git -C "$W" ls-tree -r HEAD | grep -v -P '\t(dlls/[^/]+/unix/|server/|loader/)' | sha256sum | cut -c1-12)
# The tools only by what builds them, so a DLL change keeps them.
TP=$(git -C "$W" ls-tree -r HEAD | grep -P '\t(tools/|include/|libs/|nls/|configure|aclocal.m4|Makefile.in$)' | sha256sum | cut -c1-12)
# The display libraries are keyed by what builds them; the wasm64 tree by
# those, the link and gex-gl's guest library (linked into the loader).
DH=$( { realpath "$GEX_WASM64_SYSROOT"; cat "$here"/wcc64 "$here"/../toolchain/sysroot/flags64.sh "$here"/deps64.sh "$here"/libs.pins \
	"$here"/ffi64/*; } | sha256sum | cut -c1-10)
# dpnsvr and vcruntime140_1 (configure enables them for x86_64, not wasm64,
# which serves x86-64 programs), and dmsynth with its FluidSynth (C, enabled
# only with a C++ compiler): x86-64 builtins as wasm64 modules too (part of S)
B64_MODS="--enable-dpnsvr --enable-vcruntime140_1 --enable-dmsynth --enable-fluidsynth"
# WINE64_MEDIA=lazy (the default): winegstreamer (Media Foundation's and
# DirectShow's decoders and media source, and the WM reader) over FFmpeg
# rather than GStreamer: its Unix side the Wine tree's wg_*_ffmpeg.c and
# unixlib_ffmpeg.c with FFmpeg (from deps64), a side module beside the loader
# that the loader dlopens on a program's first media call (link-wine64.sh),
# so a page fetches and compiles FFmpeg only then (the loader grows by the
# exports it needs, 16 KB); its PE side unchanged: native from the wasm trees
# (configured with the module, which configure leaves out without
# GStreamer), and its PE halves from a tree of their own (wgs) for the
# install's PE build and the prefix, where they are registered. -wgf in the
# name. WINE64_MEDIA=off leaves winegstreamer out of both arches (-nogst),
# as does the memory64 flavour (no side module of its own yet), and so does
# the default for a Wine tree without the FFmpeg files.
MEDIA=${WINE64_MEDIA:-default}
case $MEDIA in
0|off) MEDIA=0 ;;
default) MEDIA=lazy; [ -f "$W/dlls/winegstreamer/unixlib_ffmpeg.c" ] || { MEDIA=0; log "winegstreamer left out: $W has no dlls/winegstreamer/unixlib_ffmpeg.c"; }
	[ "${WINE64_MEM64:-0}" = 0 ] || MEDIA=0 ;;
lazy) [ -f "$W/dlls/winegstreamer/unixlib_ffmpeg.c" ] || { log "WINE64_MEDIA=lazy, and $W has no dlls/winegstreamer/unixlib_ffmpeg.c"; exit 1; }
	[ "${WINE64_MEM64:-0}" = 0 ] || MEDIA=0 ;;
*) log "WINE64_MEDIA=$MEDIA (lazy, the default, or off)"; exit 1 ;;
esac
if [ "$MEDIA" != 0 ]; then WGST=-wgf; B64_MODS="$B64_MODS --enable-winegstreamer"; else WGST=-nogst; fi
# WINE64_PADS=lazy (the default): game controllers. winebus's Unix side in the
# loader, its SDL bus over SDL2 (from deps64) as libSDL2-2.0.so.0, a side
# module beside the loader in the site's rest bundle (gex-rest-only), and
# the prefix keeps winebus's root device, but with "DemandStart"=1 on its
# service: services.exe does not start it at boot, so a session with no
# controller runs no winedevice for it and fetches no SDL. The page starts it
# (`sc start winebus`, WineControllers in page/runtime/src/input.js) once a controller is
# there. -sdl in the name. WINE64_PADS=off leaves all three out, as does
# the memory64 flavour and the default for a Wine tree whose services.exe
# has no DemandStart (it would start winebus at every boot).
PADS=${WINE64_PADS:-default}
case $PADS in
0|off) PADS=0 ;;
default) PADS=lazy; grep -q DemandStart "$W/programs/services/services.c" || { PADS=0; log "winebus left out: $W's services.exe has no DemandStart"; }
	[ "${WINE64_MEM64:-0}" = 0 ] || PADS=0 ;;
lazy) grep -q DemandStart "$W/programs/services/services.c" || { log "WINE64_PADS=lazy, and $W's services.exe has no DemandStart"; exit 1; }
	[ "${WINE64_MEM64:-0}" = 0 ] || PADS=0 ;;
*) log "WINE64_PADS=$PADS (lazy, the default, or off)"; exit 1 ;;
esac
PADT=; [ "$PADS" = 0 ] || { PADT=-sdl; B64_MODS="$B64_MODS --with-sdl"; }
S=$( { echo "$DH $B64_MODS"; cat "$here"/link-unix64.sh "$here"/side-imports.py "$here"/dlreg.py "$here"/../guest/gl/*.c "$here"/../guest/gl/*.h \
	"$here"/../guest/gl/*.inc "$here"/oss/sys/soundcard.h; } | sha256sum | cut -c1-10)
TLS=${WINE64_TLS:-1}; [ "$TLS" != 0 ] || S=$S-notls
# WINE64_WOW64=1: WoW64, 32-bit programs in the 64-bit process: the PE side
# built for i386 too (lib/wine/i386-windows from this commit, whose
# __wine_unix_call codes are this Unix side's) and a wow64 prefix
# (syswow64). P gets -wow64-F (F: the i386 CFLAGS below); the default
# build is unchanged.
# The i386 side's CFLAGS: -mpreferred-stack-boundary=2 (configure's, with
# -msse2) makes MinGW GCC 12 fail on a 64-bit integer to float conversion
# (an internal compiler error in oleaut32's VarR4FromUI8), so the stack is
# kept 16-byte aligned and only an incoming 4-byte alignment assumed, which
# is all the i386 ABI promises a callee.
# The PE side's debug information names the Wine tree and this cache by fixed
# names (/wine, /cache), so the same pins give the same bytes from any
# checkout path. The P key takes the flags' form, not the paths.
PEMAP="-ffile-prefix-map=$W=/wine -ffile-prefix-map=$C=/cache"
P=$P-pm$(echo '-ffile-prefix-map=W=/wine -ffile-prefix-map=C=/cache' | sha256sum | cut -c1-4)
WOW=${WINE64_WOW64:-0}; ARCHS=x86_64; CF32=(CROSSCFLAGS="-g -O2 $PEMAP")
WS=; if [ "$WOW" != 0 ]; then
	CF32+=(i386_CFLAGS="-g -O2 -mpreferred-stack-boundary=4 -mincoming-stack-boundary=2 $PEMAP")
	WS=-wow64-$(echo "i386_CFLAGS=-g -O2 -mpreferred-stack-boundary=4 -mincoming-stack-boundary=2" | sha256sum | cut -c1-6); P=$P$WS; ARCHS=i386,x86_64
	S=$S-wow64  # the Unix side registers its libraries' WoW64 tables (link-wine64.sh)
fi
# The install leaves Win16 and VxD out (no 16-bit program runs on gex): the
# 16-bit modules (.dll16 .drv16 .exe16 .mod16), the VxDs, winevdm (the
# 16-bit program host), and wow32 and w32skrnl, which import krnl386.exe16
# and so load only beside it. WINE_WIN16=1 keeps them, as before (the
# install's name then has no -no16).
WIN16_GLOB='*.dll16 *.drv16 *.exe16 *.mod16 *.vxd winevdm.exe wow32.dll w32skrnl.dll'
W16=; [ "$WOW" = 0 ] || [ "${WINE_WIN16:-0}" != 0 ] || W16=-no16-$(echo "$WIN16_GLOB" | sha256sum | cut -c1-4)
# icu (Windows' ICU API), with icuin and icuuc, which only forward to it: a
# native wasm64 module like the rest (Wine's C++ libraries, built without
# exceptions). WINE_ICU=0 leaves them out of both arches (-noicu in the name).
ICU_GLOB='icu.dll icuin.dll icuuc.dll'
WICU=; [ "${WINE_ICU:-1}" != 0 ] || WICU=-noicu
# ICU's data (share/wine/nls/icudtl.dat) with the locale data of
# WINE_ICU_LOCALES only (and their parents and root; icudat.py: the converters,
# normalization, break rules, time zones and the other non-locale data stay).
# all: every locale, as Wine ships it (no -icuL in the name).
ICU_LOCALES=${WINE_ICU_LOCALES:-en en_US}
WIL=; [ -n "$WICU" ] || [ "$ICU_LOCALES" = all ] || WIL=-icuL$( { echo "$ICU_LOCALES"; cat "$here/icudat.py"; } | sha256sum | cut -c1-6)
# WINE64_MEM64=1: the memory64 flavour: the Unix
# side compiled with wcc64 -mgex-memory64 and left unlowered (S gets -m64),
# for a process with a 64-bit memory (gexc run --mem64, the page's
# ?mem64=1). The server stays the wasm32 one.
M64=${WINE64_MEM64:-0}; WCC="$here/wcc64"; WCXX="$here/wcc64++"
if [ "$M64" != 0 ]; then S=$S-m64; WCC="$WCC -mgex-memory64"; WCXX="$WCXX -mgex-memory64"; fi
without="--without-x --without-freetype --without-gstreamer --without-pulse --without-alsa --without-cups
  --without-sane --without-gphoto --without-v4l2 --without-wayland --without-vulkan --without-opengl --without-dbus
  --without-krb5 --without-gssapi --without-netapi --without-pcap --without-usb --without-sdl --without-oss
  --without-udev --without-capi --without-inotify --without-gnutls --without-ffmpeg --without-fontconfig"
step() { # NAME CMD...: run CMD in a fresh CACHE/NAME (build trees name their own path, so in place)
	local n=$1 d=$C/$1; shift
	[ -e "$d/.done" ] && return 0
	rm -rf "$d"; mkdir -p "$d"
	( cd "$d" && "$@" ) > "$C/$n.log" 2>&1 || { log "$n failed: $C/$n.log"; tail -5 "$C/$n.log" >&2; return 1; }
	touch "$d/.done"
}

tools_build() {
	"$W/configure" --enable-archs= --without-mingw --disable-tests $without
	nice -n 10 make -j"$J" __tooldeps__ nls/locale.nls
}
log "tools ($TP)"; step tools-$TP tools_build
T=$C/tools-$TP

# Wine's raster fonts, the fixed-pitch ones a console window picks (the
# tools build, without FreeType, makes no sfnt2fon)
F=$( { git -C "$W" ls-tree -r HEAD fonts tools/sfnt2fon; cat "$here/wine-fon.sh"; grep '^FREETYPE_' "$here/libs.pins"; } | sha256sum | cut -c1-10)
fon_make() {
	WINE32_DL_SEED="${WINE32_DL_SEED:-} ${WINE32_CACHE:-$HOME/.cache/gex/wine32}/dl" JOBS=$J nice -n 10 \
		"$here/wine-fon.sh" "$W" "$T/include" "$C/dl" "$PWD/fon"
}
log "raster fonts ($F)"; step fon-$F fon_make

# Built incrementally in one tree per Wine checkout and tools (make sees
# what a new commit changed: git touches only the files it rewrites), and
# its install copied into pe64-P. A pin bump that changes one DLL took
# about 40 minutes from scratch. WINE64_PE_FRESH=1 starts that tree over.
WK=$C/pe64-work-$(echo "$W $T $PEMAP" | sha256sum | cut -c1-10)$WS
pe_build() {
	local out=$PWD
	(
		flock 9
		[ "${WINE64_PE_FRESH:-0}" = 1 ] && rm -rf "$WK"
		mkdir -p "$WK"; cd "$WK"
		[ -f Makefile ] || "$W/configure" --enable-archs=$ARCHS --with-wine-tools="$T" --disable-tests $without "${CF32[@]}"
		nice -n 10 make -k -j"$J" || nice -n 10 make -j"$J"
		rm -rf install; nice -n 10 make install prefix="$WK/install"
		cp -a --reflink=auto install "$out/install"
	) 9>"$WK.lock"
}
log "$ARCHS PE DLLs ($P)"; step pe64-$P pe_build
N=$C/pe64-$P/install

# WoW64: the display driver's PE halves. A 32-bit process's user32 loads
# the i386 one from lib/wine/i386-windows; the prefix needs both in its
# system directories, since a process loads a driver only from there (the
# 64-bit one is the native core's when there is one). They need no Wayland
# libraries, but the PE tree's configure (--without-wayland: no host
# libraries) leaves the whole module out, and enabling the module there has
# `make` build its Unix half as well, which cannot build without them. A
# tree of its own, configured with the module enabled, makes the PE files
# alone; the Unix half is the loader's (link-wine64.sh, with its WoW64 table).
wwl_build() {
	"$W/configure" --enable-archs=i386,x86_64 --enable-winewayland.drv --with-wine-tools="$T" --disable-tests $without "${CF32[@]}"
	nice -n 10 make -j"$J" dlls/winewayland.drv/i386-windows/winewayland.drv dlls/winewayland.drv/x86_64-windows/winewayland.drv
}
WD=; WL=; if [ "$WOW" != 0 ]; then
	log "winewayland.drv's PE halves ($P)"; step wwl-$P wwl_build; WD=-wwl2; WL=$C/wwl-$P/dlls/winewayland.drv
fi
# winegstreamer's PE halves the same way (its Unix half is the side module)
wgs_build() {
	"$W/configure" --enable-archs=$ARCHS --enable-winegstreamer --with-wine-tools="$T" --disable-tests $without "${CF32[@]}"
	local a t=""; for a in ${ARCHS//,/ }; do t="$t dlls/winegstreamer/$a-windows/winegstreamer.dll"; done
	# shellcheck disable=SC2086
	nice -n 10 make -j"$J" $t
}
GS=; if [ "$MEDIA" != 0 ]; then
	log "winegstreamer's PE halves ($P)"; step wgs-$P wgs_build; GS=$C/wgs-$P/dlls/winegstreamer
fi

pfx_make() {
	# The native Wine makes the prefix; a bare server of its own, no display.
	# As the page's user (gex), with an empty home: the profile then has no
	# link to the build machine's user folders (Downloads -> ~/Downloads).
	mkdir -p home
	env WINEPREFIX="$PWD/pfx" WINEARCH=win64 WINEDEBUG=-all DISPLAY= WAYLAND_DISPLAY= \
		USER=gex LOGNAME=gex HOME="$PWD/home" XDG_CONFIG_HOME="$PWD/home/.config" \
		timeout 900 nice -n 10 "$N/bin/wine" wineboot -i
	env WINEPREFIX="$PWD/pfx" "$N/bin/wineserver" -w
	rm -rf home
	grep -q '^#arch=win64' pfx/system.reg
	: > pfx/.gex-dists
}
# Each prefix keeps the keys of the installs it was made for (.gex-dists,
# prefix-key.sh's): those this script prints it with. One without the file
# predates the keys, so it is made again, never taken as it is.
fresh() { [ -f "$C/$1/pfx/.gex-dists" ] || rm -f "$C/$1/.done"; }
# -gex: made as user gex (before, as the build machine's user).
PX=$P-gex
fresh pfx64-$PX
log "win64 prefix ($PX)"; step pfx64-$PX pfx_make

# The session 32-bit Wine's prefixes have (wine32-session.reg):
# the 64-bit loader has no Unix side for winebus, wineusb or winebth
# either, so services.exe started a winedevice for each only to see
# DriverEntry fail (c0000135), one after another, before wineboot let the
# program start (a 64-bit Wine game 5 s sooner). MountMgr has one (its
# DriverEntry succeeds): demand-start in the 32-bit session, at boot again
# in 64-bit Wine's (wine64-session.reg), whose programs list the drives. On a copy, so the session changes without a new wineboot.
# And winedbg's crash dialog off (Wine's reg.exe, the native Wine): a crash
# a debugger attaches to ends with its text report, not a dialog that waits
# for a click.
# And 64-bit Wine's own (wine64-session.reg): no winemenubuilder process
# in each session, as 32-bit Wine's dist has none.
# WoW64: the display driver in system32 and syswow64 (wwl's: the prefix's
# Wine has none), where a process loads it from.
# winegstreamer's halves there too, registered (its classes, filters and
# transforms: DllRegisterServer is PE code alone, and its DllMain starts
# without a Unix side), by the native Wine's regsvr32 of each arch.
R=$( (cat "$here"/session.reg "$here"/wine64-session.reg "$here"/reg-apply.py; echo ShowCrashDialog=0; [ -z "$WD" ] || echo wwl; [ -z "$GS" ] || echo wgs; [ "$PADS" = 0 ] || echo pads) | sha256sum | cut -c1-10)
pfs_make() {
	cp -a "$C/pfx64-$PX/pfx" pfx
	[ -z "$WD" ] || cp -p "$WL/i386-windows/winewayland.drv" pfx/drive_c/windows/syswow64/
	[ -z "$WD" ] || cp -p "$WL/x86_64-windows/winewayland.drv" pfx/drive_c/windows/system32/
	if [ "$WOW" != 0 ]; then
		printf '[HKEY_LOCAL_MACHINE\\Software\\Wow6432Node\\Microsoft\\Windows NT\\CurrentVersion\\AeDebug]\n"Debugger"=-\n' > aedebug32.reg
		python3 "$here/reg-apply.py" pfx/system.reg aedebug32.reg && rm -f aedebug32.reg || return 1
	fi
	if [ "$PADS" = 0 ]; then python3 "$here/reg-apply.py" pfx/system.reg "$here/session.reg" "$here/wine64-session.reg"
	else
		# winebus's root device kept, its driver started on demand only
		{ grep -v -i '^\[-HKEY_LOCAL_MACHINE\\System\\CurrentControlSet\\Enum\\ROOT\\WINE\\WINEBUS\]' "$here/session.reg"
		  printf '\n[HKEY_LOCAL_MACHINE\\System\\CurrentControlSet\\Services\\winebus]\n"DemandStart"=dword:00000001\n'; } > session.reg
		grep -q -i 'ROOT\\WINE\\WINEBUS' session.reg && { echo "session.reg still removes winebus's root device"; return 1; }
		python3 "$here/reg-apply.py" pfx/system.reg session.reg "$here/wine64-session.reg" && rm -f session.reg
		grep -q -i '\\\\Enum\\\\ROOT\\\\WINE\\\\WINEBUS\]' pfx/system.reg || { echo "the prefix has no winebus root device"; return 1; }
	fi
	if [ -n "$GS" ]; then
		cp -p "$GS/x86_64-windows/winegstreamer.dll" pfx/drive_c/windows/system32/
		env WINEPREFIX="$PWD/pfx" WINEDEBUG=-all DISPLAY= WAYLAND_DISPLAY= \
			timeout 300 nice -n 10 "$N/bin/wine" regsvr32 /s winegstreamer.dll
		if [ "$WOW" != 0 ]; then
			cp -p "$GS/i386-windows/winegstreamer.dll" pfx/drive_c/windows/syswow64/
			env WINEPREFIX="$PWD/pfx" WINEDEBUG=-all DISPLAY= WAYLAND_DISPLAY= \
				timeout 300 nice -n 10 "$N/bin/wine" 'C:\windows\syswow64\regsvr32.exe' /s winegstreamer.dll
		fi
	fi
	env WINEPREFIX="$PWD/pfx" WINEDEBUG=-all DISPLAY= WAYLAND_DISPLAY= \
		timeout 300 nice -n 10 "$N/bin/wine" reg add 'HKCU\Software\Wine\WineDbg' /v ShowCrashDialog /t REG_DWORD /d 0 /f
	env WINEPREFIX="$PWD/pfx" "$N/bin/wineserver" -w
	grep -q '^"ShowCrashDialog"=dword:00000000' pfx/user.reg
	[ -z "$GS" ] || [ "$(grep -c 'winegstreamer.dll"$' pfx/system.reg)" -ge 10 ] || { echo "winegstreamer was not registered"; return 1; }
	: > pfx/.gex-dists
}
fresh pfs64-$PX-$R
log "win64 prefix's session ($PX, $R)"; step pfs64-$PX-$R pfs_make

# Each prefix names the builtins it was made for (.gex-builtins, "ARCH NAME"
# lines): its Wine's, and those pfs_make adds (winegstreamer's halves, the
# display driver). page-prefix.sh refuses a prefix whose list lacks a
# builtin of the install it is used with: one made for another install (a
# MEDIA=0 dist's beside a dist with winegstreamer) would boot without it
# registered, since the template is stamped current and wineboot never
# updates it. Written beside a cached prefix that predates the list too.
builtins_list() { # PFX [NAME...]: N's lib/wine/*-windows names, and each NAME in each arch N has
	local pfx=$1 a; shift
	for a in x86_64 i386; do
		[ -d "$N/lib/wine/$a-windows" ] || continue
		{ ls "$N/lib/wine/$a-windows"; printf '%s\n' "$@"; } | grep -Ev '\.(a|so|wasm|wasmref|m64)$' | sort -u | sed "s/^/$a /"
	done > "$pfx/.gex-builtins.tmp" && mv "$pfx/.gex-builtins.tmp" "$pfx/.gex-builtins"
}
[ -f "$C/pfx64-$PX/pfx/.gex-builtins" ] || builtins_list "$C/pfx64-$PX/pfx"
# shellcheck disable=SC2046
[ -f "$C/pfs64-$PX-$R/pfx/.gex-builtins" ] || builtins_list "$C/pfs64-$PX-$R/pfx" $([ -z "$GS" ] || echo winegstreamer.dll) $([ -z "$WD" ] || echo winewayland.drv)

deps64_build() {
	WINE32_DL_SEED="${WINE32_DL_SEED:-} ${WINE32_CACHE:-$HOME/.cache/gex/wine32}/dl" JOBS=$J nice -n 10 "$here/deps64.sh" "$PWD" "$C/dl"
}
log "display libraries ($DH)"; step deps64-$DH deps64_build
DEPS=$C/deps64-$DH

b64_build() {
	# FreeType, Wayland and gex-gl's EGL from deps64 (the sonames: dlreg's names)
	# and GnuTLS (crypt32's and schannel's, unless WINE64_TLS=0)
	local o=${without//--without-wayland/}; o=${o//--without-freetype/}; o=${o//--without-opengl/}
	[ "$PADS" = 0 ] || o=${o//--without-sdl/}
	[ "$TLS" = 0 ] || o="${o//--without-gnutls/} --with-gnutls"
	# wcc64: the Wine trees' paths as /wine, not this host's.
	export GEX_WCC_MAP="$W=/wine:$PWD=/wine-build:$T=/wine-tools:$DEPS=/wine-deps"
	PKG_CONFIG_LIBDIR=$DEPS/install/lib/pkgconfig "$W/configure" --host=wasm64-unknown-linux-musl --with-wine-tools="$T" \
		--disable-tests --without-mingw --enable-archs= --without-unwind $o $B64_MODS \
		CC="$WCC" CXX="$WCXX" PKG_CONFIG=pkg-config WAYLAND_SCANNER="$DEPS/host/wayland-scanner" \
		ac_cv_lib_soname_xkbregistry=libxkbregistry.so.0 ac_cv_lib_soname_freetype=libfreetype.so.6 \
		ac_cv_lib_soname_EGL=libEGL.so.1 ac_cv_lib_soname_gnutls=libgnutls.so.30 ac_cv_lib_soname_SDL2=libSDL2-2.0.so.0
	mkdir -p out
	WINE64_MEM64=$M64 WINE64_WOW64=$WOW WINE64_MEDIA=$MEDIA WINE64_PADS=$PADS WINE64_DEPS=$DEPS/install JOBS=$J "$here/link-unix64.sh" "$PWD" "$PWD/out/wine"
}
log "wasm64 Unix side ($K, $S)"; step b64-$K-$S b64_build

srv_conf() {
	GEX_WASM_SYSROOT=$SR32 "$here/build32.sh" configure "$W" "$T" "$PWD" "$DEPS32"
}
log "wineserver's wasm32 configure ($TP, $SC)"; step s32-$TP-$SC srv_conf
srv_link() {
	grep -q '^#define HAVE_SYS_INOTIFY_H 1' "$C/s32-$TP-$SC/include/config.h" ||
		{ echo "wine64-dist: s32-$TP-$SC was configured without inotify" >&2; return 1; }
	GEX_WASM_SYSROOT=$SR32 "$here/link-wineserver.sh" "$C/s32-$TP-$SC" "$W" "$PWD/obj" "$PWD"
}
log "wineserver ($K, $V)"; step srv-$K-$V srv_link

dist_make() {
	local d=$PWD
	mkdir -p lib/wine/x86_64-unix share
	cp -p "$C/b64-$K-$S"/out/wine* lib/wine/x86_64-unix/
	# (gex-rest-only: a site keeps the side modules out of its boot bundle, in
	# the rest bundle, fetched on the first dlopen: site/site-bundle.mjs)
	rm -f lib/wine/x86_64-unix/gex-rest-only
	[ -z "$GS" ] || { cp -p "$C/b64-$K-$S"/out/winegstreamer.so lib/wine/x86_64-unix/ &&
		echo winegstreamer.so >> lib/wine/x86_64-unix/gex-rest-only; }
	[ "$PADS" = 0 ] || { cp -p "$C/b64-$K-$S"/out/libSDL2-2.0.so.0 lib/wine/x86_64-unix/ &&
		echo libSDL2-2.0.so.0 >> lib/wine/x86_64-unix/gex-rest-only; }
	cp -p "$C/srv-$K-$V"/lib/wine/i386-unix/wineserver* lib/wine/x86_64-unix/
	cp -a "$N/lib/wine/x86_64-windows" lib/wine/
	[ "$WOW" = 0 ] || cp -a "$N/lib/wine/i386-windows" lib/wine/
	[ -z "$WD" ] || cp -p "$WL/i386-windows/winewayland.drv" lib/wine/i386-windows/
	[ -z "$WD" ] || cp -p "$WL/x86_64-windows/winewayland.drv" lib/wine/x86_64-windows/
	[ -z "$W16" ] || (cd lib/wine/i386-windows && rm -f $WIN16_GLOB)
	[ -z "$WICU" ] || for a in x86_64-windows i386-windows; do [ ! -d lib/wine/$a ] || (cd lib/wine/$a && rm -f $ICU_GLOB); done
	if [ -n "$GS" ]; then for a in ${ARCHS//,/ }; do cp -p "$GS/$a-windows/winegstreamer.dll" lib/wine/$a-windows/; done
	else for a in x86_64-windows i386-windows; do rm -f lib/wine/$a/winegstreamer.dll; done; fi
	cp -a "$N/share/wine" share/
	[ -z "$WIL" ] || { python3 "$here/icudat.py" share/wine/nls/icudtl.dat share/wine/nls/icudtl.dat.t $ICU_LOCALES &&
		mv share/wine/nls/icudtl.dat.t share/wine/nls/icudtl.dat; }
	# Wine's own fonts (the x86_64 build, without FreeType, installs none;
	# win32u's FreeType reads them, as 32-bit Wine's wine32/stage.sh stages them):
	# the .ttf and the raster .fon made from them
	mkdir -p share/wine/fonts && cp -f "$W"/fonts/*.ttf "$C/fon-$F"/fon/*.fon share/wine/fonts/
	ln -sfn ../../share "$d/lib/wine/share"
	# crypt32's root store, the first of its known locations
	if [ "$TLS" != 0 ]; then mkdir -p etc/ssl/certs && cp -f "$DEPS/install/ca-certificates.crt" etc/ssl/certs/; fi
}
log "install ($K, $S, $F, $V$WS$WD$W16$WICU$WIL$WGST$PADT)"; step dist64-$K-$S-f$F-s$V$WS$WD$W16$WICU$WIL$WGST$PADT dist_make

ntdll_native() {
	# The native PE ntdll (LLP64 wasm64 behind a PE32+ skeleton), made
	# in the wasm64 tree: the other DLLs stay translated
	( cd "$C/b64-$K-$S" && nice -n 10 make -j"$J" dlls/ntdll/wasm64-windows/ntdll.dll )
	cp -p "$C/b64-$K-$S"/dlls/ntdll/wasm64-windows/ntdll.dll .
}
# (the lone PE ntdll: the lowered flavour's only)
[ "$M64" != 0 ] || { log "native ntdll ($K, $S)"; step ntdll64-$K-$S ntdll_native; }

# The native PE side, the install's default: the PE builtins as native wasm
# modules (pelink.py, stage-native64.sh) in place of their x86-64 code. The
# core, WINE64_NATIVE_DLLS, is one module (winepe.wasm); every other module the
# wasm64 tree builds that the install has is a module of its own beside it
# (WINE64_SPLIT_DLLS names them instead). No builtin is left x86 code: the
# install fails when any PE image still holds machine code (pescan.py), unless
# WINE64_NATIVE_PARTIAL=1. The install's name then gets -nNK.
# WINE64_NATIVE_PE=0: the PE builtins as they are, translated (for comparison;
# the core is still made).
NPE=${WINE64_NATIVE_PE:-1}
D=${WINE64_NATIVE_DLLS:-ntdll.dll kernelbase.dll kernel32.dll msvcrt.dll ucrtbase.dll winewayland.drv}
# WoW64's own DLLs, and the programs every process start runs, in the core too
# (the twin's memory64 half: the same core and the same split modules, so
# that each skeleton names a module both halves have)
M64N=0; [ "$M64" = 0 ] || [ "${WINE64_M64_NATIVE:-0}" = 0 ] || M64N=1
if [ -z "${WINE64_NATIVE_DLLS:-}" ] && [ "$NPE" != 0 ] && { [ "$M64" = 0 ] || [ "$M64N" = 1 ]; }; then
	[ "$WOW" = 0 ] || D="$D wow64.dll wow64win.dll wow64cpu.dll"
	D="$D wineboot.exe services.exe explorer.exe"
fi
SP=${WINE64_SPLIT_DLLS:-}
SPA=; [ -n "$SP" ] || [ "$NPE" = 0 ] || { [ "$M64" != 0 ] && [ "$M64N" = 0 ]; } || SPA=all
# WINE64_EXPORT_ALL (default 1): each core (both arches) exports every
# export of its DLLs, so that a program linked later against the install
# (gexcc-win's, whose imports are DLL!name) finds whatever it calls.
# WINE64_EXPORT_ALL=0: only what the install's own modules need, plus with
# WINE64_EXPORTS=FILE the DLL!name lines it lists (a site's programs'
# imports). Either changes the cores, so it is part of their keys.
XK=
if [ "${WINE64_EXPORT_ALL:-1}" != 0 ]; then
	export PELINK_EXPORT_ALL=1; unset PELINK_EXPORTS; XK=" export-all"
else
	unset PELINK_EXPORT_ALL PELINK_EXPORTS
	if [ -n "${WINE64_EXPORTS:-}" ]; then
		[ -f "$WINE64_EXPORTS" ] || { log "no WINE64_EXPORTS file $WINE64_EXPORTS"; exit 1; }
		PELINK_EXPORTS=$(realpath "$WINE64_EXPORTS"); export PELINK_EXPORTS
		XK=" exports $(sort -u "$PELINK_EXPORTS" | sha256sum | cut -c1-10)"
	fi
fi
NK=$( { echo "$S $D${SP:+ split $SP}${SPA:+ split all$W16$WICU$WGST}$XK"; cat "$here"/pelink.py "$here"/pesym.py "$here"/stage-native.sh; } | sha256sum | cut -c1-10)
DIST=$C/dist64-$K-$S-f$F-s$V$WS$WD$W16$WICU$WIL$WGST$PADT
pe_native() {
	# The native PE core: each DLL's relocatable object and export list
	# from the wasm64 tree (winegcc leaves them beside the DLL), one module
	local b=$C/b64-$K-$S t sp=$SP
	t="libs/gexrt/libgexrt.a"
	for dll in $D; do t="$t $(pe_target "$b" "$dll")"; done
	if [ -n "$SPA" ]; then
		# every PE module, as far as each builds; one that fails is still
		# machine code, which native_make's scan refuses
		( cd "$b" && nice -n 10 make -k -j"$J" ) || true
		sp=$(cd "$b" && ls dlls/*/*.so.r.o programs/*/*.so.r.o 2>/dev/null | sed 's#.*/##; s#\.so\.r\.o$##' | sort -u |
			while read -r m; do case " $D " in *" $m "*) continue ;; esac; [ -e "$DIST/lib/wine/x86_64-windows/$m" ] && echo "$m"; done)
	fi
	for dll in $sp; do t="$t $(pe_target "$b" "$dll")"; done
	# shellcheck disable=SC2086
	( cd "$b" && nice -n 10 make -j"$J" $t ) || return 1
	echo "$sp" > split.txt
	stage_split "$b" "$W" "$PWD/native" $D
}
# stage-native64.sh ARGS over the split modules in split.txt. A split module
# importing what no module exports (a source the wasm tree does not build) is
# named and left out, so that the rest link and native_make's scan lists every
# module still machine code in one build.
stage_split() {
	local i bad
	for i in 1 2 3; do
		# shellcheck disable=SC2046
		WINE64_SPLIT_DLLS="$(echo $(cat split.txt))" nice -n 10 "$here/stage-native.sh" "$@" 2> stage.err && { cat stage.err >&2; return 0; }
		cat stage.err >&2
		bad=$(sed -n 's/^  \([^ :]*\)\.wasm: .*/\1/p' stage.err | grep -vx winepe) || return 1
		log "NOT NATIVE: imports no module exports: $(echo $bad)"
		grep -vxF -f <(echo "$bad") split.txt > split.tmp || true
		mv split.tmp split.txt
	done
	return 1
}
pe_target() { # BUILD DLL: the tree's make target for DLL's wasm64 build
	# a module named with its extension (msacm32.drv) has its own directory
	if [ -d "$1/dlls/$2" ]; then echo "dlls/$2/wasm64-windows/$2"
	elif [ -d "$1/dlls/${2%.*}" ]; then echo "dlls/${2%.*}/wasm64-windows/$2"
	elif [ -d "$1/programs/$2" ]; then echo "programs/$2/wasm64-windows/$2"
	else echo "programs/${2%.*}/wasm64-windows/$2"; fi
}
NATIVE=$C/pen64-$K-$NK/native
# WoW64's i386 PE side native as well: a wasm32 build tree of this Wine tree
# (wine32/wine-build.sh wasm, with 32-bit Wine's sysroot and libraries and
# this tree's tools), its modules staged as the x86_64 ones are
# (stage-native64.sh --i386): the core, WINE64_NATIVE_DLLS32, one module, and
# every other module the tree builds that the install's i386-windows has, one
# of its own. The install's name gets -N32 too.
N32=; NATIVE32=
if [ "$WOW" != 0 ] && [ "$NPE" != 0 ] && [ "$M64" = 0 ]; then
	# (SR32, DEPS32: the 32-bit prerequisites above)
	B32K=$( { echo "$SR32 $DEPS32${GS:+ winegstreamer}"; cat "$here"/build32.sh "$here"/wcc; } | sha256sum | cut -c1-10)
	D32=${WINE64_NATIVE_DLLS32:-ntdll.dll kernelbase.dll kernel32.dll msvcrt.dll ucrtbase.dll}
	N32=$( { echo "$B32K $D32 split all$W16$WICU$WGST$XK"; cat "$here"/pelink.py "$here"/pesym.py "$here"/stage-native.sh; } | sha256sum | cut -c1-10)
	NATIVE32=$C/pen32-$K-$N32/native
fi
# (winegstreamer's PE side: configure takes enable_winegstreamer from the
# environment as from --enable-winegstreamer, and 32-bit Wine's own tree,
# which the same script configures, stays as it is)
b32_build() {
	if [ "$MEDIA" != 0 ]; then export enable_winegstreamer=yes; fi
	GEX_WASM_SYSROOT=$SR32 WINE32_JOBS=$J nice -n 10 "$here/build32.sh" wasm "$W" "$T" "$PWD" "$DEPS32"
}
pe_native32() {
	local b=$C/b32-$K-$B32K sp
	sp=$(cd "$b" && ls dlls/*/*.so.r.o programs/*/*.so.r.o 2>/dev/null | sed 's#.*/##; s#\.so\.r\.o$##' | sort -u |
		while read -r m; do case " $D32 " in *" $m "*) continue ;; esac; [ -e "$DIST/lib/wine/i386-windows/$m" ] && echo "$m"; done)
	echo "$sp" > split.txt
	# shellcheck disable=SC2086
	GEX_WASM_SYSROOT=$SR32 stage_split --i386 "$b" "$W" "$PWD/native" $D32
}
if [ "$M64" != 0 ]; then
	# The memory64 flavour: its loader; with WINE64_M64_NATIVE=1 (the twin's
	# half) its PE core too, a module for a 64-bit memory (pelink.py sees
	# the tree's -mgex-memory64). Otherwise nothing more than before.
	if [ "${WINE64_M64_NATIVE:-0}" != 0 ]; then
		log "native PE core ($K, $NK)"; step pen64-$K-$NK pe_native
	fi
	"$here/prefix-key.sh" --add "$C/pfx64-$PX/pfx" "$DIST" || exit 1
	echo "WINE64_DIST=$DIST"; echo "WINE64_PREFIX=$C/pfx64-$PX/pfx"; echo "WINE64_TOOLS=$T"; echo "WINE64_SRC=$W"
	echo "WINE64_BUILD=$C/b64-$K-$S"
	[ "${WINE64_M64_NATIVE:-0}" = 0 ] || echo "WINE64_NATIVE=$NATIVE"
	exit 0
fi
log "native PE core ($K, $NK)"; step pen64-$K-$NK pe_native
if [ -n "$N32" ]; then
	log "i386 wasm32 tree ($K, $B32K)"; step b32-$K-$B32K b32_build
	log "i386 native PE side ($K, $N32)"; step pen32-$K-$N32 pe_native32
fi
# The install with the native PE side in place of the PE builtins (rm first:
# the copy's files are hard links)
native_make() {
	cp -al "$DIST/." .
	rm -f .done
	local a n
	# each skeleton exports every name its DLL's PE build (still in place
	# here) does, but for those its spec keeps from the wasm arch
	for a in x86_64:$NATIVE i386:$NATIVE32; do
		n=${a#*:}; [ -n "$n" ] || continue
		python3 "$here/skel-exports.py" --against "lib/wine/${a%%:*}-windows" "$n" "$W" "${a%%:*}" >&2 ||
			{ log "skeletons short of their PE build's exports (above)"; return 1; }
	done
	for a in x86_64:$NATIVE i386:$NATIVE32; do
		n=${a#*:}; [ -n "$n" ] || continue
		for f in "$n"/*; do [ -f "$f" ] || continue; rm -f "lib/wine/${a%%:*}-windows/${f##*/}"; cp -p "$f" "lib/wine/${a%%:*}-windows/"; done
	done
	# a release install's modules without the names gex never reads (and
	# producers, debug info): gexcc-win's stripnames.py, as its programs'
	if [ "$STRIP" != 0 ]; then
		find lib/wine/x86_64-windows ${NATIVE32:+lib/wine/i386-windows} -maxdepth 1 -type f \( -name '*.wasm' -o -name '*.wasm.m64' \) -print0 |
			xargs -0 -r -P"$J" -n 16 sh -c 'for f; do python3 "$0" "$f" "$f.s" && mv "$f.s" "$f" || exit 1; done' "$here/../toolchain/gexcc/windows/stripnames.py" ||
			{ log "stripnames failed"; return 1; }
	fi
	# no builtin runs translated: a PE image left with machine code fails the
	# dist (WINE64_NATIVE_PARTIAL=1 keeps it, while a module is being ported).
	if [ -n "$SPA" ]; then
		python3 "$here/pescan.py" lib/wine/x86_64-windows ${NATIVE32:+lib/wine/i386-windows} >&2 ||
			[ "${WINE64_NATIVE_PARTIAL:-0}" = 1 ] || { log "PE images with machine code (above): the dist is not native"; return 1; }
	fi
}
# WINE64_STRIP=0: the native modules keep their names, for a debug install
# (its name gets -dbg)
STRIP=${WINE64_STRIP:-1}
if [ "$NPE" != 0 ]; then
	NI=-n$NK${N32:+-$N32}
	if [ "$STRIP" = 0 ]; then NI=$NI-dbg; else NI=$NI-s$(sha256sum < "$here/../toolchain/gexcc/windows/stripnames.py" | cut -c1-6); fi
	log "native install ($K, $NK${N32:+, $N32})"; step dist64-$K-$S-f$F-s$V$WS$WD$W16$WICU$WIL$WGST$PADT$NI native_make
	DIST=$C/dist64-$K-$S-f$F-s$V$WS$WD$W16$WICU$WIL$WGST$PADT$NI
fi
# WINE64_TWIN=1: a twin install (per program): the default
# install with the memory64 loader beside the lowered one, as
# x86_64-unix/wine.m64 (the WINE64_MEM64=1 build's wine; one PE side, one
# prefix). An exec of the loader that asks for a 64-bit memory (GEX_MEM64=1)
# runs the twin (shim vfork.rs `exec_route`); every other exec, the default.
# The native PE core likewise: the memory64 build's module beside the
# default's, as winepe.wasm.m64, which a process with a 64-bit memory loads
# in its place (one set of skeletons: the same DLLs and exports).
# Off by default: the default install and its downloads do not grow.
BASE=$DIST
if [ "${WINE64_TWIN:-0}" != 0 ]; then
	MO=$(WINE64_TWIN=0 WINE64_MEM64=1 WINE64_M64_NATIVE=1 "$0" "$W" "$C") || { log "the memory64 build for the twin failed"; exit 1; }
	MD=$(sed -n 's/^WINE64_DIST=//p' <<< "$MO"); MN=$(sed -n 's/^WINE64_NATIVE=//p' <<< "$MO")
	[ -n "$MD" ] && [ -f "$MN/winepe.wasm" ] || { log "the memory64 build for the twin made no loader or PE core"; exit 1; }
	MK=${MN%/native}; MK=${MK##*-}
	twin_make() {
		cp -a "$DIST/." .
		rm -f .done
		cp -p "$MD/lib/wine/x86_64-unix/wine" lib/wine/x86_64-unix/wine.m64
		# the memory64 build's modules, which a process with a 64-bit memory
		# loads in its own modules' place: the install runs the twin as it is
		local m
		for m in "$MN"/*.wasm; do cp -p "$m" "lib/wine/x86_64-windows/${m##*/}.m64"; done
	}
	# (named after the install it copies and the memory64 build: a twin of
	# another install, or of another memory64 build, is another twin)
	log "twin install ($K, $S, $F, $V, $MK)"; step "${DIST##*/}-twin$MK" twin_make
	DIST=$DIST-twin$MK
	twin_native() {
		cp -a "$NATIVE/." .
		rm -f .done
		# every module of the memory64 build's, the core and the split ones
		local m
		for m in "$MN"/*.wasm; do cp -p "$m" "${m##*/}.m64"; done
	}
	log "twin native PE core ($K, $NK, $MK)"; step pen64-$K-$NK-twin$MK twin_native
	NATIVE=$C/pen64-$K-$NK-twin$MK
fi
for d in "$BASE" "$DIST"; do "$here/prefix-key.sh" --add "$C/pfs64-$PX-$R/pfx" "$d" || exit 1; done
echo "WINE64_DIST=$DIST"
echo "WINE64_PREFIX=$C/pfs64-$PX-$R/pfx"
echo "WINE64_TOOLS=$T"
echo "WINE64_SRC=$W"
echo "WINE64_BUILD=$C/b64-$K-$S"
echo "WINE64_NTDLL=$C/ntdll64-$K-$S/ntdll.dll"
echo "WINE64_SERVER=$C/srv-$K-$V"
echo "WINE64_NATIVE=$NATIVE"
[ -z "$NATIVE32" ] || echo "WINE64_NATIVE32=$NATIVE32"
# (the i386 wasm32 tree and its sysroot, for a program linked into the i386
# core as a builtin: test/wow64/ptval32)
[ -z "$NATIVE32" ] || { echo "WINE64_BUILD32=$C/b32-$K-$B32K"; echo "WINE64_SYSROOT32=$SR32"; }
