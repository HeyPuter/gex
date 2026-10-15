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
#   tools-P     Wine's build tools, native (widl and winebuild know wasm64)
#   pe64-P      the x86_64 PE DLLs, MinGW, and native64 their install (with
#               a native x86_64 Unix side, used only to make the prefix)
#   pfx64-P     a win64 prefix (wineboot -i, native, no display)
#   b64-K-S     the wasm64 build tree of the Unix side (wcc64)
#   srv-K       the wasm32 wineserver (link-wineserver.sh over W3_BUILD)
#   dist64-K-S  the install: x86_64-unix/{wine,wineserver}, x86_64-windows,
#               share/wine
#   ntdll64-K-S the native PE ntdll (W64.2), made in b64-K-S
#   pen64-K-N   the native PE core (W64.2): W64_NATIVE_DLLS (default ntdll,
#               kernelbase, kernel32, msvcrt, ucrtbase) made in b64-K-S and
#               linked as one module behind their skeletons
#               (stage-native64.sh)
# K is the Wine commit, P the Wine tree less its Unix side (tools, pe64,
# pfx64), S the sysroot and gex's link tools, N those, the PE linker and
# the DLL list.
# Env: GEX_WASM64_SYSROOT (build-sysroot64.sh); JOBS (default 16).
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
S=$( { realpath "$GEX_WASM64_SYSROOT"; cat "$here"/link-w64.sh "$here"/wcc64 "$here"/flags64.sh "$here"/dlreg.py; } | sha256sum | cut -c1-10)
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
log "tools ($P)"; step tools-$P tools_build
T=$C/tools-$P

pe_build() {
	"$W/configure" --enable-archs=x86_64 --with-wine-tools="$T" --disable-tests $without
	nice -n 10 make -k -j"$J" || nice -n 10 make -j"$J"
	nice -n 10 make install prefix="$PWD/install"
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

b64_build() {
	"$W/configure" --host=wasm64-unknown-linux-musl --with-wine-tools="$T" --disable-tests --without-mingw \
		--enable-archs= --without-unwind $without CC="$here/wcc64" CXX="$here/wcc64++"
	mkdir -p out
	JOBS=$J "$here/link-w64.sh" "$PWD" "$PWD/out/wine"
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
	ln -sfn ../../share "$d/lib/wine/share"
}
log "install ($K, $S)"; step dist64-$K-$S dist_make

ntdll_native() {
	# W64.2's native PE ntdll (LLP64 wasm64 behind a PE32+ skeleton), made
	# in the wasm64 tree: the other DLLs stay translated
	( cd "$C/b64-$K-$S" && nice -n 10 make -j"$J" dlls/ntdll/wasm64-windows/ntdll.dll )
	cp -p "$C/b64-$K-$S"/dlls/ntdll/wasm64-windows/ntdll.dll .
}
log "native ntdll ($K, $S)"; step ntdll64-$K-$S ntdll_native

D=${W64_NATIVE_DLLS:-ntdll.dll kernelbase.dll kernel32.dll msvcrt.dll ucrtbase.dll}
NK=$( { echo "$S $D"; cat "$here"/pelink.py "$here"/pesym.py "$here"/stage-native64.sh; } | sha256sum | cut -c1-10)
pe_native() {
	# W64.2's native PE core: each DLL's relocatable object and export list
	# from the wasm64 tree (winegcc leaves them beside the DLL), one module
	local b=$C/b64-$K-$S t
	t="libs/gexrt/libgexrt.a"
	for dll in $D; do t="$t dlls/${dll%.*}/wasm64-windows/$dll"; done
	# shellcheck disable=SC2086
	( cd "$b" && nice -n 10 make -j"$J" $t )
	# shellcheck disable=SC2086
	nice -n 10 "$here/stage-native64.sh" "$b" "$W" "$PWD/native" $D
}
log "native PE core ($K, $NK)"; step pen64-$K-$NK pe_native
echo "W64_DIST=$C/dist64-$K-$S"
echo "W64_PREFIX=$C/pfx64-$P/pfx"
echo "W64_TOOLS=$T"
echo "W64_SRC=$W"
echo "W64_NTDLL=$C/ntdll64-$K-$S/ntdll.dll"
echo "W64_NATIVE=$C/pen64-$K-$NK/native"
