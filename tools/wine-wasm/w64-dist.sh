#!/bin/bash
# A W64 install and a win64 prefix for it (docs/wine-native-x64.md §5,
# W64.1), built from a Wine tree with the W64 work (Wine branch w64/main):
#
#   tools/wine-wasm/w64-dist.sh WINE CACHE
#
# prints W64_DIST=..., W64_PREFIX=..., and W64_TOOLS=... and W64_SRC=...
# (the native tools build and the Wine tree: the native PE side's case),
# for tests/wine64/nt-x64.sh, W64_NTDLL=... (the native PE ntdll, W64.2) and
# W64_NATIVE=... (the native PE core, W64.2: nt-x64.sh's NATIVE=).
# Each step is cached in CACHE by what it depends on, and skipped when done:
#   tools-TP    Wine's build tools, native (widl and winebuild know wasm64);
#               TP: tools/, include/, libs/, nls/ and the configure inputs
#   pe64-P      the x86_64 PE DLLs, MinGW, and native64 their install, made
#               in pe64-work-* incrementally (with
#               a native x86_64 Unix side, used only to make the prefix)
#   pfx64-P     a win64 prefix (wineboot -i, native, no display)
#   pfs64-P-R   it with W3's session applied (w3-session.reg; R: that and
#               reg-apply.py): W64_PREFIX
#   deps64-D    the Unix side's display libraries, LP64 wasm64 (w64-deps.sh:
#               FreeType, libwayland, libxml2, xkbcommon, EGL headers; W64.4;
#               GnuTLS over nettle and Mozilla's CA bundle, as W3's)
#   b64-K-S     the wasm64 build tree of the Unix side (wcc64), with them:
#               win32u (+ FreeType), winewayland.drv and opengl32 (+ gex-gl's
#               EGL, guest/gl/gexgl.c; W64.4), secur32 (+ GnuTLS) and
#               wineoss.drv (audio, against gex-kernel's /dev/dsp, as W3's)
#               in the loader; W64_TLS=0 leaves GnuTLS out (S gets -notls)
#   srv-K       the wasm32 wineserver (link-wineserver.sh over W3_BUILD)
#   dist64-K-S-twin  with W64_TWIN=1, that install plus the memory64
#               loader as x86_64-unix/wine.m64 (the W64_MEM64=1 build's)
#   dist64-K-S  the install: x86_64-unix/{wine,wineserver}, x86_64-windows,
#               share/wine, and etc/ssl/certs/ca-certificates.crt (crypt32's
#               root store; natively the host's /etc is the guest's)
#   ntdll64-K-S the native PE ntdll (W64.2), made in b64-K-S
#   pen64-K-N   the native PE core (W64.2): W64_NATIVE_DLLS (default ntdll,
#               kernelbase, kernel32, msvcrt, ucrtbase, and winewayland.drv,
#               the display driver's PE side, W64.4) made in b64-K-S and
#               linked as one module behind their skeletons
#               (stage-native64.sh)
#   pen64-K-N-twin  with W64_TWIN=1, that core plus the memory64 build's
#               module as winepe.wasm.m64 (W64_NATIVE then names it)
# K is the Wine commit, P the Wine tree less its Unix side (tools, pe64,
# pfx64), D the sysroot, the compiler and the libraries' pins and build,
# S those, gex's link tools, gex-gl's guest library and the OSSv4 header
# (w3/oss), N those, the PE
# linker and the DLL list.
# Env: GEX_WASM64_SYSROOT (build-sysroot64.sh); JOBS (default 16); the
# libraries' tarballs are kept in CACHE/dl, taken from W3_DL_SEED and
# W3's downloads (W3_CACHE, default ~/.cache/gex-w3) when there.
# The server is compiled with a configured W3 build tree's commands:
# W3_BUILD and GEX_WASM_SYSROOT, or, unset, tools/wine-wasm's own (`make
# paths`, under W3_CACHE), made first when missing (`make build`: the W3
# sysroot, libraries and Wine tree, an hour or more the first time) --
# checked before anything else is built. Needs x86_64-w64-mingw32-gcc. The
# first run builds Wine twice (native and MinGW, then wasm64): about 40
# minutes on 16 threads.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
W=$(cd "${1:?WINE}" && pwd); mkdir -p "${2:?CACHE}"; C=$(cd "$2" && pwd)
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
J=${JOBS:-16}
[ -z "$(git -C "$W" status --porcelain --untracked-files=no)" ] || { echo "w64-dist: $W has uncommitted changes" >&2; exit 1; }
K=$(git -C "$W" rev-parse --short=12 HEAD)
log() { echo "w64-dist: $*" >&2; }
# The server's prerequisite first, so a missing one fails now, not after
# the Wine builds: a configured W3 build tree and its sysroot.
if [ ! -e "$C/srv-$K/.done" ]; then
	own=$(make -s -C "$here" paths) || { log "make -C tools/wine-wasm paths failed"; exit 1; }
	ownb=$(sed -n 's/^W3_BUILD=//p' <<< "$own")
	if [ -z "${W3_BUILD:-}" ]; then
		W3_BUILD=$ownb; GEX_WASM_SYSROOT=$(sed -n 's/^GEX_WASM_SYSROOT=//p' <<< "$own")
	fi
	if [ "$W3_BUILD" = "$ownb" ] && ! ls "$W3_BUILD"/.done-* >/dev/null 2>&1; then
		log "no W3 build tree at $W3_BUILD: making it (make -C tools/wine-wasm build, log $C/w3-build.log)"
		nice -n 10 make -C "$here" build W3_JOBS="$J" > "$C/w3-build.log" 2>&1 ||
			{ log "the W3 build tree failed: $C/w3-build.log"; tail -5 "$C/w3-build.log" >&2; exit 1; }
	fi
	[ -f "$W3_BUILD/Makefile" ] || { log "W3_BUILD=$W3_BUILD is no configured W3 build tree (make -C tools/wine-wasm build)"; exit 1; }
	[ -d "${GEX_WASM_SYSROOT:-}" ] || { log "GEX_WASM_SYSROOT=${GEX_WASM_SYSROOT:-} is no W3 sysroot (make -C tools/wine-wasm paths)"; exit 1; }
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
DH=$( { realpath "$GEX_WASM64_SYSROOT"; cat "$here"/wcc64 "$here"/flags64.sh "$here"/w64-deps.sh "$here"/w3.pins \
	"$here"/ffi64/*; } | sha256sum | cut -c1-10)
S=$( { echo "$DH"; cat "$here"/link-w64.sh "$here"/dlreg.py "$here"/../../guest/gl/*.c "$here"/../../guest/gl/*.h \
	"$here"/../../guest/gl/*.inc "$here"/w3/oss/sys/soundcard.h; } | sha256sum | cut -c1-10)
TLS=${W64_TLS:-1}; [ "$TLS" != 0 ] || S=$S-notls
# W64_MEM64=1: the memory64 flavour (docs/wine-native-x64.md §0.1): the Unix
# side compiled with wcc64 -mgex-memory64 and left unlowered (S gets -m64),
# for a process with a 64-bit memory (gexc run --mem64, the page's
# ?mem64=1). The server stays the wasm32 one.
M64=${W64_MEM64:-0}; WCC="$here/wcc64"; WCXX="$here/wcc64++"
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

# Built incrementally in one tree per Wine checkout and tools (make sees
# what a new commit changed: git touches only the files it rewrites), and
# its install copied into pe64-P. A pin bump that changes one DLL took
# about 40 minutes from scratch. W64_PE_FRESH=1 starts that tree over.
WK=$C/pe64-work-$(echo "$W $T" | sha256sum | cut -c1-10)
pe_build() {
	local out=$PWD
	(
		flock 9
		[ "${W64_PE_FRESH:-0}" = 1 ] && rm -rf "$WK"
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

# The session W3's prefixes have (w3-session.reg, docs/wine-native.md
# §3.17): the 64-bit loader has no Unix side for MountMgr, winebus,
# wineusb or winebth either, so services.exe started a winedevice for each
# only to see DriverEntry fail (c0000135), one after another, before
# wineboot let the program start (a W64 game
# 5 s sooner). On a copy, so the session changes without a new wineboot.
# And winedbg's crash dialog off (Wine's reg.exe, the native Wine): a crash
# a debugger attaches to ends with its text report, not a dialog that waits
# for a click.
R=$( (cat "$here"/w3-session.reg "$here"/reg-apply.py; echo ShowCrashDialog=0) | sha256sum | cut -c1-10)
pfs_make() {
	cp -a "$C/pfx64-$P/pfx" pfx
	python3 "$here/reg-apply.py" pfx/system.reg "$here/w3-session.reg"
	env WINEPREFIX="$PWD/pfx" WINEDEBUG=-all DISPLAY= WAYLAND_DISPLAY= \
		timeout 300 nice -n 10 "$N/bin/wine" reg add 'HKCU\Software\Wine\WineDbg' /v ShowCrashDialog /t REG_DWORD /d 0 /f
	env WINEPREFIX="$PWD/pfx" "$N/bin/wineserver" -w
	grep -q '^"ShowCrashDialog"=dword:00000000' pfx/user.reg
}
log "win64 prefix's session ($P, $R)"; step pfs64-$P-$R pfs_make

deps64_build() {
	W3_DL_SEED="${W3_DL_SEED:-} ${W3_CACHE:-$HOME/.cache/gex-w3}/dl" JOBS=$J nice -n 10 "$here/w64-deps.sh" "$PWD" "$C/dl"
}
log "display libraries ($DH)"; step deps64-$DH deps64_build
DEPS=$C/deps64-$DH

b64_build() {
	# FreeType, Wayland and gex-gl's EGL from deps64 (the sonames: dlreg's names)
	# and GnuTLS (crypt32's and schannel's, unless W64_TLS=0)
	local o=${without//--without-wayland/}; o=${o//--without-freetype/}; o=${o//--without-opengl/}
	[ "$TLS" = 0 ] || o="${o//--without-gnutls/} --with-gnutls"
	PKG_CONFIG_LIBDIR=$DEPS/install/lib/pkgconfig "$W/configure" --host=wasm64-unknown-linux-musl --with-wine-tools="$T" \
		--disable-tests --without-mingw --enable-archs= --without-unwind $o \
		CC="$WCC" CXX="$WCXX" PKG_CONFIG=pkg-config WAYLAND_SCANNER="$DEPS/host/wayland-scanner" \
		ac_cv_lib_soname_xkbregistry=libxkbregistry.so.0 ac_cv_lib_soname_freetype=libfreetype.so.6 \
		ac_cv_lib_soname_EGL=libEGL.so.1 ac_cv_lib_soname_gnutls=libgnutls.so.30
	mkdir -p out
	W64_MEM64=$M64 W64_DEPS=$DEPS/install JOBS=$J "$here/link-w64.sh" "$PWD" "$PWD/out/wine"
}
log "wasm64 Unix side ($K, $S)"; step b64-$K-$S b64_build

srv_link() {
	"$here/link-wineserver.sh" "$W3_BUILD" "$W" "$PWD/obj" "$PWD"
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
	# win32u's FreeType reads them, as W3's w3/stage.sh stages them)
	mkdir -p share/wine/fonts && cp -f "$W"/fonts/*.ttf share/wine/fonts/
	ln -sfn ../../share "$d/lib/wine/share"
	# crypt32's root store, the first of its known locations
	if [ "$TLS" != 0 ]; then mkdir -p etc/ssl/certs && cp -f "$DEPS/install/ca-certificates.crt" etc/ssl/certs/; fi
}
log "install ($K, $S)"; step dist64-$K-$S dist_make

ntdll_native() {
	# W64.2's native PE ntdll (LLP64 wasm64 behind a PE32+ skeleton), made
	# in the wasm64 tree: the other DLLs stay translated
	( cd "$C/b64-$K-$S" && nice -n 10 make -j"$J" dlls/ntdll/wasm64-windows/ntdll.dll )
	cp -p "$C/b64-$K-$S"/dlls/ntdll/wasm64-windows/ntdll.dll .
}
# (W64.2's lone PE ntdll: the lowered flavour's only)
[ "$M64" != 0 ] || { log "native ntdll ($K, $S)"; step ntdll64-$K-$S ntdll_native; }

D=${W64_NATIVE_DLLS:-ntdll.dll kernelbase.dll kernel32.dll msvcrt.dll ucrtbase.dll winewayland.drv}
NK=$( { echo "$S $D"; cat "$here"/pelink.py "$here"/pesym.py "$here"/stage-native64.sh; } | sha256sum | cut -c1-10)
pe_native() {
	# W64.2's native PE core: each DLL's relocatable object and export list
	# from the wasm64 tree (winegcc leaves them beside the DLL), one module
	local b=$C/b64-$K-$S t
	t="libs/gexrt/libgexrt.a"
	for dll in $D; do t="$t dlls/${dll%.dll}/wasm64-windows/$dll"; done
	# shellcheck disable=SC2086
	( cd "$b" && nice -n 10 make -j"$J" $t ) || return 1
	# shellcheck disable=SC2086
	nice -n 10 "$here/stage-native64.sh" "$b" "$W" "$PWD/native" $D
}
DIST=$C/dist64-$K-$S
NATIVE=$C/pen64-$K-$NK/native
if [ "$M64" != 0 ]; then
	# The memory64 flavour: its loader; with W64_M64_NATIVE=1 (the twin's
	# half) its PE core too, a module for a 64-bit memory (pelink.py sees
	# the tree's -mgex-memory64). Otherwise nothing more than before.
	if [ "${W64_M64_NATIVE:-0}" != 0 ]; then
		log "native PE core ($K, $NK)"; step pen64-$K-$NK pe_native
	fi
	echo "W64_DIST=$DIST"; echo "W64_PREFIX=$C/pfx64-$P/pfx"; echo "W64_TOOLS=$T"; echo "W64_SRC=$W"
	[ "${W64_M64_NATIVE:-0}" = 0 ] || echo "W64_NATIVE=$NATIVE"
	exit 0
fi
log "native PE core ($K, $NK)"; step pen64-$K-$NK pe_native
# W64_TWIN=1: a twin install (docs/mem64.md §5, per program): the default
# install with the memory64 loader beside the lowered one, as
# x86_64-unix/wine.m64 (the W64_MEM64=1 build's wine; one PE side, one
# prefix). An exec of the loader that asks for a 64-bit memory (GEX_MEM64=1)
# runs the twin (shim vfork.rs `exec_route`); every other exec, the default.
# The native PE core likewise: the memory64 build's module beside the
# default's, as winepe.wasm.m64, which a process with a 64-bit memory loads
# in its place (one set of skeletons: the same DLLs and exports).
# Off by default: the default install and its downloads do not grow.
if [ "${W64_TWIN:-0}" != 0 ]; then
	MO=$(W64_TWIN=0 W64_MEM64=1 W64_M64_NATIVE=1 "$0" "$W" "$C") || { log "the memory64 build for the twin failed"; exit 1; }
	MD=$(sed -n 's/^W64_DIST=//p' <<< "$MO"); MN=$(sed -n 's/^W64_NATIVE=//p' <<< "$MO")
	[ -n "$MD" ] && [ -f "$MN/winepe.wasm" ] || { log "the memory64 build for the twin made no loader or PE core"; exit 1; }
	twin_make() {
		cp -a "$DIST/." .
		rm -f .done
		cp -p "$MD/lib/wine/x86_64-unix/wine" lib/wine/x86_64-unix/wine.m64
	}
	log "twin install ($K, $S)"; step dist64-$K-$S-twin twin_make
	DIST=$C/dist64-$K-$S-twin
	twin_native() {
		cp -a "$NATIVE/." .
		rm -f .done
		cp -p "$MN/winepe.wasm" winepe.wasm.m64
	}
	log "twin native PE core ($K, $NK)"; step pen64-$K-$NK-twin twin_native
	NATIVE=$C/pen64-$K-$NK-twin
fi
echo "W64_DIST=$DIST"
echo "W64_PREFIX=$C/pfs64-$P-$R/pfx"
echo "W64_TOOLS=$T"
echo "W64_SRC=$W"
echo "W64_NTDLL=$C/ntdll64-$K-$S/ntdll.dll"
echo "W64_NATIVE=$NATIVE"
