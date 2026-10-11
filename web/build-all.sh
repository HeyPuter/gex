#!/bin/sh
# Build the page, for the multi-process build (MP, the default), the
# single-thread build (SP, docs/single-thread.md), or both from one tree.
#
#   web/build-all.sh [--mp | --sp | --both] [--no-assets] [--no-gl] [--no-x]
#
# WHAT IS SHARED AND WHAT IS NOT
#
# The two flavours run the same runtime: SP loads the *shared* shim, kernel and
# seed (gex_browser.shared.wasm, gex_kernel.wasm, seed.shared.wasm) and clears
# their memories' `shared` flag as it compiles them (single.mjs
# enableSingle). Which one a page runs is decided at load time (runtime.js
# singleMode: no SharedArrayBuffer, or `?single=1`), so one vite bundle in
# web/dist serves both. Building those twice would make two identical copies
# that could only drift. They are built once, by web/build-assets.sh.
#
# What differs is native code compiled with or without -pthread. Each flavour
# has its own build directory and its own published directory, so neither
# overwrites the other:
#
#   Mesa (gex-wgpu-replay)  MP: $CACHE/mesa-mt -> web/public/gex/
#                           SP: $CACHE/mesa-st -> web/public/gex/st/
#   Xwayland                MP: $CACHE/xwl-mt  -> web/public/gex/xwayland/
#                           SP: $CACHE/xwl-st  -> web/public/gex/xwayland-st/
#   Xwayland host tools     both: $CACHE/xwl-host (only if the distro lacks them)
#
# $CACHE is GEX_WEB_BUILD (default target/web-build), so build trees survive
# across runs:
# - Mesa is incremental through meson and ninja.
# - Xwayland's script always builds from scratch, so it is skipped when a
#   stamp of its inputs matches what was last published.
#
# Serve both at once with `node web/serve-dist.mjs 8088 --sp-port 8089`: MP
# with COOP/COEP on 8088, SP without on 8089.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CACHE="${GEX_WEB_BUILD:-$ROOT/target/web-build}"
MP=1 SP=0 ASSETS=1 GL=1 X=1
for a in "$@"; do
    case $a in
    --mp) MP=1 SP=0 ;;
    --sp) MP=0 SP=1 ;;
    --both) MP=1 SP=1 ;;
    --no-assets) ASSETS=0 ;;
    --no-gl) GL=0 ;;
    --no-x) X=0 ;;
    *) echo "usage: web/build-all.sh [--mp | --sp | --both] [--no-assets] [--no-gl] [--no-x]" >&2; exit 2 ;;
    esac
done
mkdir -p "$CACHE"
say() { printf '\n== %s\n' "$*"; }

# -- shared by both: shim (plain and shared), kernel, seeds, providers, rootfs
if [ "$ASSETS" = 1 ]; then
    say "runtime and assets (both flavours)"
    "$HERE/build-assets.sh"
fi

# -- Mesa, per flavour ---------------------------------------------------------
mesa() { # mesa <mt|st>
    t=1; [ "$1" = st ] && t=0
    say "Mesa ($1)"
    GEX_MESA_THREADS=$t "$HERE/build-mesa-wasm.sh" "$CACHE/mesa-$1" > "$CACHE/mesa-$1.log" 2>&1 ||
        { tail -30 "$CACHE/mesa-$1.log"; echo "Mesa ($1) failed; log in $CACHE/mesa-$1.log" >&2; exit 1; }
    grep -a "published" "$CACHE/mesa-$1.log" || true
}

# -- Xwayland, per flavour, skipped when its inputs have not changed ---------
xwl_key() { # xwl_key <mt|st>
    {
        echo "$1 ${XWAYLAND_TAG:-} ${WAYLAND_TAG:-}"
        cat "$HERE/build-xwayland-wasm.sh"
        find "$HERE/xwayland" "$ROOT/shims/hostwl" -type f -not -path '*/target/*' | LC_ALL=C sort | xargs cat
    } | sha256sum | cut -c1-32
}
xwl_host_tools() {
    # The distro's, when it has them all; otherwise built once into the cache.
    if pkg-config --exists wayland-scanner wayland-protocols xwaylandproto 2>/dev/null &&
       [ -f /usr/share/aclocal/xorg-macros.m4 ] && [ -f /usr/include/X11/xshmfence.h ]; then
        return 0
    fi
    "$HERE/build-xwayland-host-tools.sh" "$CACHE/xwl-host"
    GEX_XWL_HOST_TOOLS="$CACHE/xwl-host"; export GEX_XWL_HOST_TOOLS
}
xwl() { # xwl <mt|st>
    t=1 pub=xwayland; [ "$1" = st ] && t=0 pub=xwayland-st
    key=$(xwl_key "$1")
    if [ "$(cat "$CACHE/xwl-$1.stamp" 2>/dev/null)" = "$key" ] && [ -f "$ROOT/web/public/gex/$pub/Xwayland.wasm" ]; then
        say "Xwayland ($1): unchanged, web/public/gex/$pub kept"
        return 0
    fi
    say "Xwayland ($1)"
    xwl_host_tools
    GEX_XWL_THREADS=$t "$HERE/build-xwayland-wasm.sh" "$CACHE/xwl-$1" > "$CACHE/xwl-$1.log" 2>&1 ||
        { tail -30 "$CACHE/xwl-$1.log"; echo "Xwayland ($1) failed; log in $CACHE/xwl-$1.log" >&2; exit 1; }
    echo "$key" > "$CACHE/xwl-$1.stamp"
    tail -2 "$CACHE/xwl-$1.log"
}

[ "$MP" = 1 ] && [ "$GL" = 1 ] && mesa mt
[ "$SP" = 1 ] && [ "$GL" = 1 ] && mesa st
[ "$MP" = 1 ] && [ "$X" = 1 ] && xwl mt
[ "$SP" = 1 ] && [ "$X" = 1 ] && xwl st

# -- one bundle for both -------------------------------------------------------
say "web/dist"
(cd "$HERE" && npm run --silent build > "$CACHE/vite.log" 2>&1) ||
    { tail -20 "$CACHE/vite.log"; exit 1; }
echo "  web/dist: built (serves MP when cross-origin isolated, SP when not)"
