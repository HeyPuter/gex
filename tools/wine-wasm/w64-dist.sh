#!/bin/bash
# A W64 install and a win64 prefix for it (docs/wine-native-x64.md §5,
# W64.1), built from a Wine tree with the W64 work (Wine branch w64/main):
#
#   tools/wine-wasm/w64-dist.sh WINE CACHE
#
# prints W64_DIST=... and W64_PREFIX=... (for tests/wine64/nt-x64.sh).
# Each step is cached in CACHE by what it depends on, and skipped when done:
#   tools-P     Wine's build tools, native (widl and winebuild know wasm64)
#   pe64-P      the x86_64 PE DLLs, MinGW, and native64 their install (with
#               a native x86_64 Unix side, used only to make the prefix)
#   pfx64-P     a win64 prefix (wineboot -i, native, no display)
#   b64-K-S     the wasm64 build tree of the Unix side (wcc64)
#   srv-K       the wasm32 wineserver (link-wineserver.sh over W3_BUILD)
#   dist64-K-S  the install: x86_64-unix/{wine,wineserver}, x86_64-windows,
#               share/wine
# K is the Wine commit, P the Wine tree less its Unix side (tools, pe64,
# pfx64), S the sysroot and gex's link tools.
# Env: GEX_WASM64_SYSROOT (build-sysroot64.sh); W3_BUILD, a configured W3
# build tree, and GEX_WASM_SYSROOT, the W3 sysroot (tools/wine-wasm's
# `make paths`), for the server; JOBS (default 16). Needs
# x86_64-w64-mingw32-gcc. The first run builds Wine twice (native and
# MinGW, then wasm64): about 40 minutes on 16 threads.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
W=$(cd "${1:?WINE}" && pwd); mkdir -p "${2:?CACHE}"; C=$(cd "$2" && pwd)
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
: "${W3_BUILD:?set W3_BUILD to a configured W3 Wine build tree (tools/wine-wasm make paths)}"
: "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT to the W3 sysroot}"
J=${JOBS:-16}
[ -z "$(git -C "$W" status --porcelain --untracked-files=no)" ] || { echo "w64-dist: $W has uncommitted changes" >&2; exit 1; }
K=$(git -C "$W" rev-parse --short=12 HEAD)
# The tools, the PE DLLs and the prefix do not depend on the Unix side
# (dlls/*/unix, server, loader): keyed by the rest of the tree, so a
# Unix-side change rebuilds only the wasm64 tree and the server.
P=$(git -C "$W" ls-tree -r HEAD | grep -v -P '\t(dlls/[^/]+/unix/|server/|loader/)' | sha256sum | cut -c1-12)
S=$( { realpath "$GEX_WASM64_SYSROOT"; cat "$here"/link-w64.sh "$here"/wcc64 "$here"/flags64.sh "$here"/dlreg.py; } | sha256sum | cut -c1-10)
log() { echo "w64-dist: $*" >&2; }
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
echo "W64_DIST=$C/dist64-$K-$S"
echo "W64_PREFIX=$C/pfx64-$P/pfx"
