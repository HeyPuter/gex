#!/bin/sh
# Build the page for the multi-process build (MP, the default), the
# single-thread build (SP, docs/single-thread.md), or both, from one tree.
#
#   web/build-all.sh [--mp | --sp | --both] [--debug]
#                    [--no-assets] [--no-gl] [--no-x] [--dist-only]
#
# Each flavour gets its own dist:
#
#   MP  web/dist      served with COOP/COEP; threaded Mesa and Xwayland
#   SP  web/dist-sp   served without them; non-pthread Mesa and Xwayland
#
# Neither dist holds the other flavour's Mesa or Xwayland. They are about
# 35 MB a flavour (Mesa 20 MB, Xwayland and its bridge the rest).
#
# THE STEPS
#
# 1. The runtime both flavours run, built once into web/public by
#    web/build-assets.sh:
#    - the shim, kernel and seed, which the page loads as
#      gex_browser.shared.wasm, gex_kernel.wasm and seed.shared.wasm;
#    - the providers, rootfs and guest GL libraries.
#    SP loads the same three files and clears their memories' `shared` flag
#    as it compiles them (single.mjs enableSingle). An SP-only shim cannot be
#    smaller: its per-fiber stacks and TLS are what the atomics build provides
#    (docs/single-thread.md §9).
# 2. Mesa and Xwayland for each flavour, each in its own build tree:
#
#      Mesa      MP: $CACHE/mesa-mt -> web/public/gex/
#                SP: $CACHE/mesa-st -> web/public/gex/st/
#      Xwayland  MP: $CACHE/xwl-mt  -> web/public/gex/xwayland/
#                SP: $CACHE/xwl-st  -> web/public/gex/xwayland-st/
#
#    Mesa rebuilds incrementally through ninja. The Xwayland script always
#    builds from scratch, so it is skipped while a stamp of its inputs
#    matches.
# 3. For each flavour, a staged public tree in $CACHE/public-<flavour>:
#    - hard links to web/public;
#    - without the other flavour's Mesa and Xwayland;
#    - without the files no page loads: the plain shim, seed and kernel,
#      which run-node.mjs and the check scripts use from web/public.
#    Its large wasm files are stripped of custom sections (DWARF, names) unless
#    --debug is given. The unstripped files stay in web/public and are
#    linked into target/web-debug/<flavour>/ for symbolizing. vite then
#    builds the flavour's dist from that tree.
#
# $CACHE is GEX_WEB_BUILD, by default target/web-build.
#
# To serve both at once:
#
#   node web/serve-dist.mjs 8088 --sp-port 8089
#
# This serves web/dist (MP) on 8088 and web/dist-sp (SP) on 8089.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CACHE="${GEX_WEB_BUILD:-$ROOT/target/web-build}"
DEBUGDIR="$ROOT/target/web-debug"
MP=1 SP=0 ASSETS=1 GL=1 X=1 DEBUG=0
for a in "$@"; do
    case $a in
    --mp) MP=1 SP=0 ;;
    --sp) MP=0 SP=1 ;;
    --both) MP=1 SP=1 ;;
    --debug) DEBUG=1 ;;
    --no-assets) ASSETS=0 ;;
    --no-gl) GL=0 ;;
    --no-x) X=0 ;;
    --dist-only) ASSETS=0 GL=0 X=0 ;;
    *) echo "usage: web/build-all.sh [--mp | --sp | --both] [--debug] [--no-assets] [--no-gl] [--no-x] [--dist-only]" >&2; exit 2 ;;
    esac
done
mkdir -p "$CACHE"
say() { printf '\n== %s\n' "$*"; }

# -- 1. shared by both: shim, kernel, seeds, providers, rootfs ----------------
if [ "$ASSETS" = 1 ]; then
    say "runtime and assets (both flavours)"
    # build-assets.sh ends by refreshing web/dist through refresh-dist.sh,
    # which would call back into this script.
    GEX_NO_REFRESH_DIST=1 "$HERE/build-assets.sh"
fi

# -- 2. Mesa and Xwayland, per flavour ----------------------------------------
mesa() { # mesa <mt|st>
    t=1; [ "$1" = st ] && t=0
    say "Mesa ($1)"
    GEX_MESA_THREADS=$t "$HERE/build-mesa-wasm.sh" "$CACHE/mesa-$1" > "$CACHE/mesa-$1.log" 2>&1 ||
        { tail -30 "$CACHE/mesa-$1.log"; echo "Mesa ($1) failed; log in $CACHE/mesa-$1.log" >&2; exit 1; }
    grep -a "published" "$CACHE/mesa-$1.log" || true
}
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

# -- 3. one dist per flavour ---------------------------------------------------
dist() { # dist <mp|sp>
    f=$1
    out=$HERE/dist; [ "$f" = sp ] && out=$HERE/dist-sp
    stage=$CACHE/public-$f
    say "web/${out##*/} ($f)"
    rm -rf "$stage" "$DEBUGDIR/$f"
    cp -al "$HERE/public" "$stage"
    # Only what this flavour's page loads (runtime.js: the shared shim, the
    # shared seed, gex_kernel.wasm), and only its own Mesa and Xwayland.
    rm -f "$stage"/gex/gex_kernel*.plain.wasm
    for w in "$stage"/gex/gex_browser*.wasm "$stage"/gex/seed*.wasm; do
        case $w in *.shared.wasm) ;; *) rm -f "$w" ;; esac
    done
    if [ "$f" = mp ]; then
        rm -rf "$stage/gex/st" "$stage/gex/xwayland-st"
    else
        rm -rf "$stage/gex/xwayland" "$stage"/gex/gex-wgpu-replay.*
    fi
    if [ "$DEBUG" = 0 ]; then
        # Custom sections out: DWARF, and the name section, which nothing in
        # the runtime reads (a trap then names wasm-function[N]; symbolize
        # with the copies in target/web-debug). Not the seed or the providers,
        # whose gex.* sections the runtime does read.
        mkdir -p "$DEBUGDIR/$f"
        for w in "$stage"/gex/gex_browser*.shared.wasm "$stage"/gex/gex_kernel*.wasm \
                 "$stage"/gex/gex-wgpu-replay.wasm "$stage"/gex/st/gex-wgpu-replay.wasm \
                 "$stage"/gex/xwayland*/Xwayland.wasm "$stage"/gex/xwayland*/gexhostwl.wasm; do
            [ -f "$w" ] || continue
            rel=${w#"$stage"/}
            mkdir -p "$DEBUGDIR/$f/$(dirname "$rel")"
            ln -f "$w" "$DEBUGDIR/$f/$rel" 2>/dev/null || cp "$w" "$DEBUGDIR/$f/$rel"
            # A new file, not a write through the hard link into web/public.
            wasm-tools strip -a "$w" -o "$w.strip" && mv -f "$w.strip" "$w"
        done
    fi
    (cd "$HERE" && GEX_PUBLIC_DIR="$stage" GEX_OUT_DIR="$out" npm run --silent build > "$CACHE/vite-$f.log" 2>&1) ||
        { tail -20 "$CACHE/vite-$f.log"; exit 1; }
    rm -rf "$stage"
    echo "  web/${out##*/}: $(du -sh "$out" | cut -f1)"
}
if [ "$DEBUG" = 0 ] && ! command -v wasm-tools >/dev/null 2>&1; then
    echo "need wasm-tools to strip the dists (cargo install wasm-tools), or pass --debug" >&2
    exit 1
fi
[ "$MP" = 1 ] && dist mp
[ "$SP" = 1 ] && dist sp
exit 0
