#!/bin/sh
# Host-side pieces web/build-xwayland-wasm.sh needs that a distro may lack:
# wayland-scanner (a build tool), wayland-protocols (XML), xorgproto's newer
# protocol headers (xwaylandproto), xorg-macros (for the autotools libraries'
# autoreconf) and X11/xshmfence.h (miext/sync/misyncshm.c includes it; nothing
# links its code). Built from source into a prefix, for either flavour of the
# Xwayland build:
#
#   web/build-xwayland-host-tools.sh [prefix]     # default target/web-build/xwl-host
#   GEX_XWL_HOST_TOOLS=<prefix> web/build-xwayland-wasm.sh
#
# Skips everything when <prefix>/.done matches this script, so it is cheap to
# call on every build (web/build-all.sh does).
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
P="${1:-$ROOT/target/web-build/xwl-host}"
S="$P/src"
KEY=$(sha256sum "$0" | cut -c1-16)
[ "$(cat "$P/.done" 2>/dev/null)" = "$KEY" ] && { echo "xwayland host tools: cached in $P"; exit 0; }
rm -rf "$P"; mkdir -p "$S"
cd "$S"
clone() { git clone -q --depth 1 --branch "$2" "$1" "$3"; }
clone https://gitlab.freedesktop.org/wayland/wayland.git 1.23.1 wayland
clone https://gitlab.freedesktop.org/wayland/wayland-protocols.git 1.41 wayland-protocols
clone https://gitlab.freedesktop.org/xorg/proto/xorgproto.git xorgproto-2024.1 xorgproto
clone https://gitlab.freedesktop.org/xorg/util/macros.git util-macros-1.20.1 util-macros
clone https://gitlab.freedesktop.org/xorg/lib/libxshmfence.git libxshmfence-1.3.2 libxshmfence
b() {
    d=$1; shift
    meson setup "$S/build-$d" "$S/$d" --prefix="$P" "$@" > "$S/log-$d.txt" 2>&1 &&
        ninja -C "$S/build-$d" install >> "$S/log-$d.txt" 2>&1 ||
        { tail -20 "$S/log-$d.txt"; exit 1; }
    echo "  built $d"
}
b wayland -Dlibraries=false -Ddocumentation=false -Dtests=false -Ddtd_validation=false
export PKG_CONFIG_PATH="$P/lib/x86_64-linux-gnu/pkgconfig:$P/share/pkgconfig:$P/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export PATH="$P/bin:$PATH"
b wayland-protocols -Dtests=false
b xorgproto
(cd util-macros && autoreconf -fi && ./configure --prefix="$P" && make install) > "$S/log-util-macros.txt" 2>&1 ||
    { tail -20 "$S/log-util-macros.txt"; exit 1; }
echo "  built util-macros"
mkdir -p "$P/include/X11"
cp libxshmfence/src/xshmfence.h "$P/include/X11/"
echo "$KEY" > "$P/.done"
echo "xwayland host tools: built into $P"
