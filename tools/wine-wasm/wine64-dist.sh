#!/bin/bash
# A 64-bit Wine install and a win64 prefix for it,
# built from a Wine tree with the 64-bit Wine work (wine64.pins names the
# commit; another is built too, with a note):
#
#   tools/wine-wasm/wine64-dist.sh WINE CACHE
#
# prints WINE64_DIST=..., WINE64_PREFIX=..., and WINE64_TOOLS=... and WINE64_SRC=...
# (the native tools build and the Wine tree: the native PE side's case),
# for a test, WINE64_NTDLL=... (the native PE ntdll) and
# WINE64_NATIVE=... (the native PE core).
# Each step is cached in CACHE by what it depends on, and skipped when done:
#   tools-TP    Wine's build tools, native (widl and winebuild know wasm64);
#               TP: tools/, include/, libs/, nls/ and the configure inputs
#   pe64-P      the x86_64 PE DLLs, MinGW, and native64 their install, made
#               in pe64-work-* incrementally (with
#               a native x86_64 Unix side, used only to make the prefix)
#   pfx64-P     a win64 prefix (wineboot -i, native, no display)
#   pfs64-P-R   it with 32-bit Wine's session applied (wine32-session.reg; R: that and
#               reg-apply.py): WINE64_PREFIX
#   deps64-D    the Unix side's display libraries, LP64 wasm64 (wine64-deps.sh:
#               FreeType, libwayland, libxml2, xkbcommon, EGL headers;
#               GnuTLS over nettle and Mozilla's CA bundle, as 32-bit Wine's)
#   b64-K-S     the wasm64 build tree of the Unix side (wcc64), with them:
#               win32u (+ FreeType), winewayland.drv and opengl32 (+ gex-gl's
#               EGL, guest/gl/gexgl.c), secur32 (+ GnuTLS) and
#               wineoss.drv (audio, against gex-kernel's /dev/dsp, as 32-bit Wine's)
#               in the loader; WINE64_TLS=0 leaves GnuTLS out (S gets -notls)
#   srv-K       the wasm32 wineserver (link-wineserver.sh over WINE32_BUILD)
#   fon-F       Wine's raster fonts (wine-fon.sh: sfnt2fon over a native
#               FreeType); F: fonts/, tools/sfnt2fon, the script and FreeType's pin
#   dist64-K-S-fF-twin  with WINE64_TWIN=1, that install plus the memory64
#               loader as x86_64-unix/wine.m64 (the WINE64_MEM64=1 build's)
#   dist64-K-S-fF  the install: x86_64-unix/{wine,wineserver}, x86_64-windows,
#               share/wine (with the .fon fonts), and etc/ssl/certs/ca-certificates.crt (crypt32's
#               root store; natively the host's /etc is the guest's)
#   ntdll64-K-S the native PE ntdll , made in b64-K-S
#   pen64-K-N   the native PE core : WINE64_NATIVE_DLLS (default ntdll,
#               kernelbase, kernel32, msvcrt, ucrtbase, and winewayland.drv,
#               the display driver's PE side) made in b64-K-S and
#               linked as one module behind their skeletons
#               (stage-native64.sh)
#   pen64-K-N-twin  with WINE64_TWIN=1, that core plus the memory64 build's
#               module as winepe.wasm.m64 (WINE64_NATIVE then names it)
# K is the Wine commit, P the Wine tree less its Unix side (tools, pe64,
# pfx64), D the sysroot, the compiler and the libraries' pins and build,
# S those, gex's link tools, gex-gl's guest library and the OSSv4 header
# (wine32/oss), N those, the PE
# linker and the DLL list.
# Env: GEX_WASM64_SYSROOT (build-sysroot64.sh); JOBS (default 16); the
# libraries' tarballs are kept in CACHE/dl, taken from WINE32_DL_SEED and
# 32-bit Wine's downloads (WINE32_CACHE, default ~/.cache/gex-wine32) when there.
# The server is compiled with a configured 32-bit Wine build tree's commands:
# WINE32_BUILD and GEX_WASM_SYSROOT, or, unset, tools/wine-wasm's own (`make
# paths`, under WINE32_CACHE), made first when missing (`make build`: the 32-bit Wine
# sysroot, libraries and Wine tree, an hour or more the first time) --
# checked before anything else is built. Needs x86_64-w64-mingw32-gcc. The
# first run builds Wine twice (native and MinGW, then wasm64): about 40
# minutes on 16 threads.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
W=$(cd "${1:?WINE}" && pwd); mkdir -p "${2:?CACHE}"; C=$(cd "$2" && pwd)
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
J=${JOBS:-16}
[ -z "$(git -C "$W" status --porcelain --untracked-files=no)" ] || { echo "wine64-dist: $W has uncommitted changes" >&2; exit 1; }
K=$(git -C "$W" rev-parse --short=12 HEAD)
pin=$(sed -n 's/^WINE64_COMMIT *:= *//p' "$here/wine64.pins")
[ "$(git -C "$W" rev-parse HEAD)" = "$pin" ] || echo "wine64-dist: $W is at $K, not wine64.pins' $(printf %.12s "$pin")" >&2
log() { echo "wine64-dist: $*" >&2; }
# The server's prerequisite first, so a missing one fails now, not after
# the Wine builds: a configured 32-bit Wine build tree and its sysroot.
if [ ! -e "$C/srv-$K/.done" ]; then
	own=$(make -s -C "$here" paths) || { log "make -C tools/wine-wasm paths failed"; exit 1; }
	ownb=$(sed -n 's/^WINE32_BUILD=//p' <<< "$own")
	if [ -z "${WINE32_BUILD:-}" ]; then
		WINE32_BUILD=$ownb; GEX_WASM_SYSROOT=$(sed -n 's/^GEX_WASM_SYSROOT=//p' <<< "$own")
	fi
	if [ "$WINE32_BUILD" = "$ownb" ] && ! ls "$WINE32_BUILD"/.done-* >/dev/null 2>&1; then
		log "no wine32 build tree at $WINE32_BUILD: making it (make -C tools/wine-wasm build, log $C/wine32-build.log)"
		nice -n 10 make -C "$here" build WINE32_JOBS="$J" > "$C/wine32-build.log" 2>&1 ||
			{ log "the wine32 build tree failed: $C/wine32-build.log"; tail -5 "$C/wine32-build.log" >&2; exit 1; }
	fi
	[ -f "$WINE32_BUILD/Makefile" ] || { log "WINE32_BUILD=$WINE32_BUILD is no configured wine32 build tree (make -C tools/wine-wasm build)"; exit 1; }
	[ -d "${GEX_WASM_SYSROOT:-}" ] || { log "GEX_WASM_SYSROOT=${GEX_WASM_SYSROOT:-} is no wine32 sysroot (make -C tools/wine-wasm paths)"; exit 1; }
	export GEX_WASM_SYSROOT
fi
# The tools, the PE DLLs and the prefix do not depend on the Unix side
# (dlls/*/unix, server, loader): keyed by the rest of the tree, so a
# Unix-side change rebuilds only the wasm64 tree and the server.
P=$(git -C "$W" ls-tree -r HEAD | grep -v -P '\t(dlls/[^/]+/unix/|server/|loader/)' | sha256sum | cut -c1-12)
# The tools only by what builds them, so a DLL change keeps them.
TP=$(git -C "$W" ls-tree -r HEAD | grep -P '\t(tools/|include/|libs/|nls/|configure|aclocal.m4|Makefile.in$)' | sha256sum | cut -c1-12)
# The display libraries are keyed by what builds them; the wasm64 tree by
# those, the link and gex-gl's guest library (linked into the loader).
DH=$( { realpath "$GEX_WASM64_SYSROOT"; cat "$here"/wcc64 "$here"/flags64.sh "$here"/wine64-deps.sh "$here"/wine32.pins \
	"$here"/ffi64/*; } | sha256sum | cut -c1-10)
S=$( { echo "$DH"; cat "$here"/link-wine64.sh "$here"/dlreg.py "$here"/../../guest/gl/*.c "$here"/../../guest/gl/*.h \
	"$here"/../../guest/gl/*.inc "$here"/wine32/oss/sys/soundcard.h; } | sha256sum | cut -c1-10)
TLS=${WINE64_TLS:-1}; [ "$TLS" != 0 ] || S=$S-notls
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
F=$( { git -C "$W" ls-tree -r HEAD fonts tools/sfnt2fon; cat "$here/wine-fon.sh"; grep '^FREETYPE_' "$here/wine32.pins"; } | sha256sum | cut -c1-10)
fon_make() {
	WINE32_DL_SEED="${WINE32_DL_SEED:-} ${WINE32_CACHE:-$HOME/.cache/gex-wine32}/dl" JOBS=$J nice -n 10 \
		"$here/wine-fon.sh" "$W" "$T/include" "$C/dl" "$PWD/fon"
}
log "raster fonts ($F)"; step fon-$F fon_make

# Built incrementally in one tree per Wine checkout and tools (make sees
# what a new commit changed: git touches only the files it rewrites), and
# its install copied into pe64-P. A pin bump that changes one DLL took
# about 40 minutes from scratch. WINE64_PE_FRESH=1 starts that tree over.
WK=$C/pe64-work-$(echo "$W $T" | sha256sum | cut -c1-10)
pe_build() {
	local out=$PWD
	(
		flock 9
		[ "${WINE64_PE_FRESH:-0}" = 1 ] && rm -rf "$WK"
		mkdir -p "$WK"; cd "$WK"
		[ -f Makefile ] || "$W/configure" --enable-archs=x86_64 --with-wine-tools="$T" --disable-tests $without
		nice -n 10 make -k -j"$J" || nice -n 10 make -j"$J"
		rm -rf install; nice -n 10 make install prefix="$WK/install"
		cp -a --reflink=auto install "$out/install"
	) 9>"$WK.lock"
}
log "x86_64 PE DLLs ($P)"; step pe64-$P pe_build
N=$C/pe64-$P/install

pfx_make() {
	# The native Wine makes the prefix; a bare server of its own, no display.
	env WINEPREFIX="$PWD/pfx" WINEARCH=win64 WINEDEBUG=-all DISPLAY= WAYLAND_DISPLAY= \
		timeout 900 nice -n 10 "$N/bin/wine" wineboot -i
	env WINEPREFIX="$PWD/pfx" "$N/bin/wineserver" -w
	grep -q '^#arch=win64' pfx/system.reg
}
log "win64 prefix ($P)"; step pfx64-$P pfx_make

# The session 32-bit Wine's prefixes have (wine32-session.reg):
# the 64-bit loader has no Unix side for MountMgr, winebus,
# wineusb or winebth either, so services.exe started a winedevice for each
# only to see DriverEntry fail (c0000135), one after another, before
# wineboot let the program start (a 64-bit Wine game
# 5 s sooner). On a copy, so the session changes without a new wineboot.
# And winedbg's crash dialog off (Wine's reg.exe, the native Wine): a crash
# a debugger attaches to ends with its text report, not a dialog that waits
# for a click.
R=$( (cat "$here"/wine32-session.reg "$here"/reg-apply.py; echo ShowCrashDialog=0) | sha256sum | cut -c1-10)
pfs_make() {
	cp -a "$C/pfx64-$P/pfx" pfx
	python3 "$here/reg-apply.py" pfx/system.reg "$here/wine32-session.reg"
	env WINEPREFIX="$PWD/pfx" WINEDEBUG=-all DISPLAY= WAYLAND_DISPLAY= \
		timeout 300 nice -n 10 "$N/bin/wine" reg add 'HKCU\Software\Wine\WineDbg' /v ShowCrashDialog /t REG_DWORD /d 0 /f
	env WINEPREFIX="$PWD/pfx" "$N/bin/wineserver" -w
	grep -q '^"ShowCrashDialog"=dword:00000000' pfx/user.reg
}
log "win64 prefix's session ($P, $R)"; step pfs64-$P-$R pfs_make

deps64_build() {
	WINE32_DL_SEED="${WINE32_DL_SEED:-} ${WINE32_CACHE:-$HOME/.cache/gex-wine32}/dl" JOBS=$J nice -n 10 "$here/wine64-deps.sh" "$PWD" "$C/dl"
}
log "display libraries ($DH)"; step deps64-$DH deps64_build
DEPS=$C/deps64-$DH

b64_build() {
	# FreeType, Wayland and gex-gl's EGL from deps64 (the sonames: dlreg's names)
	# and GnuTLS (crypt32's and schannel's, unless WINE64_TLS=0)
	local o=${without//--without-wayland/}; o=${o//--without-freetype/}; o=${o//--without-opengl/}
	[ "$TLS" = 0 ] || o="${o//--without-gnutls/} --with-gnutls"
	# wcc64: the Wine trees' paths as /wine, not this host's.
	export GEX_WCC_MAP="$W=/wine:$PWD=/wine-build:$T=/wine-tools:$DEPS=/wine-deps"
	PKG_CONFIG_LIBDIR=$DEPS/install/lib/pkgconfig "$W/configure" --host=wasm64-unknown-linux-musl --with-wine-tools="$T" \
		--disable-tests --without-mingw --enable-archs= --without-unwind $o \
		CC="$WCC" CXX="$WCXX" PKG_CONFIG=pkg-config WAYLAND_SCANNER="$DEPS/host/wayland-scanner" \
		ac_cv_lib_soname_xkbregistry=libxkbregistry.so.0 ac_cv_lib_soname_freetype=libfreetype.so.6 \
		ac_cv_lib_soname_EGL=libEGL.so.1 ac_cv_lib_soname_gnutls=libgnutls.so.30
	mkdir -p out
	WINE64_MEM64=$M64 WINE64_DEPS=$DEPS/install JOBS=$J "$here/link-wine64.sh" "$PWD" "$PWD/out/wine"
}
log "wasm64 Unix side ($K, $S)"; step b64-$K-$S b64_build

srv_link() {
	"$here/link-wineserver.sh" "$WINE32_BUILD" "$W" "$PWD/obj" "$PWD"
}
log "wineserver ($K)"; step srv-$K srv_link

dist_make() {
	local d=$PWD
	mkdir -p lib/wine/x86_64-unix share
	cp -p "$C/b64-$K-$S"/out/wine* lib/wine/x86_64-unix/
	cp -p "$C/srv-$K"/lib/wine/i386-unix/wineserver* lib/wine/x86_64-unix/
	cp -a "$N/lib/wine/x86_64-windows" lib/wine/
	cp -a "$N/share/wine" share/
	# Wine's own fonts (the x86_64 build, without FreeType, installs none;
	# win32u's FreeType reads them, as 32-bit Wine's wine32/stage.sh stages them):
	# the .ttf and the raster .fon made from them
	mkdir -p share/wine/fonts && cp -f "$W"/fonts/*.ttf "$C/fon-$F"/fon/*.fon share/wine/fonts/
	ln -sfn ../../share "$d/lib/wine/share"
	# crypt32's root store, the first of its known locations
	if [ "$TLS" != 0 ]; then mkdir -p etc/ssl/certs && cp -f "$DEPS/install/ca-certificates.crt" etc/ssl/certs/; fi
}
log "install ($K, $S, $F)"; step dist64-$K-$S-f$F dist_make

ntdll_native() {
	# The native PE ntdll (LLP64 wasm64 behind a PE32+ skeleton), made
	# in the wasm64 tree: the other DLLs stay translated
	( cd "$C/b64-$K-$S" && nice -n 10 make -j"$J" dlls/ntdll/wasm64-windows/ntdll.dll )
	cp -p "$C/b64-$K-$S"/dlls/ntdll/wasm64-windows/ntdll.dll .
}
# (the lone PE ntdll: the lowered flavour's only)
[ "$M64" != 0 ] || { log "native ntdll ($K, $S)"; step ntdll64-$K-$S ntdll_native; }

D=${WINE64_NATIVE_DLLS:-ntdll.dll kernelbase.dll kernel32.dll msvcrt.dll ucrtbase.dll winewayland.drv}
NK=$( { echo "$S $D"; cat "$here"/pelink.py "$here"/pesym.py "$here"/stage-native64.sh; } | sha256sum | cut -c1-10)
pe_native() {
	# The native PE core: each DLL's relocatable object and export list
	# from the wasm64 tree (winegcc leaves them beside the DLL), one module
	local b=$C/b64-$K-$S t
	t="libs/gexrt/libgexrt.a"
	for dll in $D; do t="$t dlls/${dll%.dll}/wasm64-windows/$dll"; done
	# shellcheck disable=SC2086
	( cd "$b" && nice -n 10 make -j"$J" $t ) || return 1
	# shellcheck disable=SC2086
	nice -n 10 "$here/stage-native64.sh" "$b" "$W" "$PWD/native" $D
}
DIST=$C/dist64-$K-$S-f$F
NATIVE=$C/pen64-$K-$NK/native
if [ "$M64" != 0 ]; then
	# The memory64 flavour: its loader; with WINE64_M64_NATIVE=1 (the twin's
	# half) its PE core too, a module for a 64-bit memory (pelink.py sees
	# the tree's -mgex-memory64). Otherwise nothing more than before.
	if [ "${WINE64_M64_NATIVE:-0}" != 0 ]; then
		log "native PE core ($K, $NK)"; step pen64-$K-$NK pe_native
	fi
	echo "WINE64_DIST=$DIST"; echo "WINE64_PREFIX=$C/pfx64-$P/pfx"; echo "WINE64_TOOLS=$T"; echo "WINE64_SRC=$W"
	[ "${WINE64_M64_NATIVE:-0}" = 0 ] || echo "WINE64_NATIVE=$NATIVE"
	exit 0
fi
log "native PE core ($K, $NK)"; step pen64-$K-$NK pe_native
# WINE64_TWIN=1: a twin install (per program): the default
# install with the memory64 loader beside the lowered one, as
# x86_64-unix/wine.m64 (the WINE64_MEM64=1 build's wine; one PE side, one
# prefix). An exec of the loader that asks for a 64-bit memory (GEX_MEM64=1)
# runs the twin (shim vfork.rs `exec_route`); every other exec, the default.
# The native PE core likewise: the memory64 build's module beside the
# default's, as winepe.wasm.m64, which a process with a 64-bit memory loads
# in its place (one set of skeletons: the same DLLs and exports).
# Off by default: the default install and its downloads do not grow.
if [ "${WINE64_TWIN:-0}" != 0 ]; then
	MO=$(WINE64_TWIN=0 WINE64_MEM64=1 WINE64_M64_NATIVE=1 "$0" "$W" "$C") || { log "the memory64 build for the twin failed"; exit 1; }
	MD=$(sed -n 's/^WINE64_DIST=//p' <<< "$MO"); MN=$(sed -n 's/^WINE64_NATIVE=//p' <<< "$MO")
	[ -n "$MD" ] && [ -f "$MN/winepe.wasm" ] || { log "the memory64 build for the twin made no loader or PE core"; exit 1; }
	twin_make() {
		cp -a "$DIST/." .
		rm -f .done
		cp -p "$MD/lib/wine/x86_64-unix/wine" lib/wine/x86_64-unix/wine.m64
	}
	log "twin install ($K, $S, $F)"; step dist64-$K-$S-f$F-twin twin_make
	DIST=$C/dist64-$K-$S-f$F-twin
	twin_native() {
		cp -a "$NATIVE/." .
		rm -f .done
		cp -p "$MN/winepe.wasm" winepe.wasm.m64
	}
	log "twin native PE core ($K, $NK)"; step pen64-$K-$NK-twin twin_native
	NATIVE=$C/pen64-$K-$NK-twin
fi
echo "WINE64_DIST=$DIST"
echo "WINE64_PREFIX=$C/pfs64-$P-$R/pfx"
echo "WINE64_TOOLS=$T"
echo "WINE64_SRC=$W"
echo "WINE64_NTDLL=$C/ntdll64-$K-$S/ntdll.dll"
echo "WINE64_NATIVE=$NATIVE"
