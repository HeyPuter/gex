#!/bin/sh
# Build Mesa's desktop OpenGL frontend for wasm32 on the driver that already
# draws these pixels: Gallium's `virgl` encoder, with virglrenderer decoding on
# the page's WebGL.
#
#   web/build-mesa-virgl-wasm.sh [out-dir]
#
# This is the same rendering path the guest has always used -- Mesa's virgl
# driver on one side of the vtest protocol, virglrenderer on the other -- with
# the guest taken out of it. Mesa runs here as wasm rather than as translated
# AArch64, and the socket between the two halves is a call into the module next
# door (see the Mesa fork's `gex_vtest_os.c`).
#
# Separate from `build-mesa-wasm.sh` because that one builds the WebGPU driver,
# which wants emdawnwebgpu and a Tint build. Nothing here needs either: the
# gex_wgpu driver is only configured when `-Dgex-emdawn-dir` is passed, so
# leaving it out is what makes this build small and quick.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
MESA=${GEX_MESA_SRC:-$ROOT/third_party/mesa}
OUT=${1:-${TMPDIR:-/tmp}/gex-mesa-virgl}
BUILD=$OUT/build
PUB=$ROOT/web/public/gex
CROSS=$ROOT/web/mesa-wasm-cross.ini

command -v meson >/dev/null 2>&1 || { echo "need meson" >&2; exit 1; }
command -v ninja >/dev/null 2>&1 || { echo "need ninja" >&2; exit 1; }
command -v emcc >/dev/null 2>&1 || { echo "need Emscripten" >&2; exit 1; }
[ -f "$MESA/meson.build" ] || {
    echo "initialize third_party/mesa" >&2
    exit 1
}
# The submodule tracks mesa-wink's `wink` branch, which has the WebGPU driver
# but not the virgl replay (gex-virgl-replay): that lives on the fork's
# `gex-virgl` branch. Say so rather than fail deep in meson.
grep -q gex-virgl-replay "$MESA/src/gallium/targets/gex-wasm/meson.build" 2>/dev/null || {
    echo "third_party/mesa has no gex-virgl-replay target: it is on mesa-wink's" >&2
    echo "gex-virgl branch, not wink (what the submodule tracks). Build from a" >&2
    echo "gex-virgl checkout: GEX_MESA_SRC=/path/to/mesa-wink@gex-virgl $0" >&2
    exit 1
}

if [ ! -f "$BUILD/build.ninja" ]; then
    mkdir -p "$OUT"
    meson setup "$BUILD" "$MESA" \
        --cross-file "$CROSS" \
        -Dbuildtype=release \
        -Dplatforms=[] \
        -Dgallium-drivers=softpipe,virgl \
        -Dvulkan-drivers=[] \
        -Dglx=disabled \
        -Degl=disabled \
        -Dgbm=disabled \
        -Dllvm=disabled \
        -Dshared-glapi=disabled \
        -Dopengl=true \
        -Dgles1=disabled \
        -Dgles2=enabled \
        -Dvideo-codecs=[] \
        -Dvalgrind=disabled \
        -Dlibunwind=disabled \
        -Dzstd=disabled \
        -Dzlib=disabled \
        -Ddisplay-info=disabled \
        -Dexpat=disabled \
        -Dspirv-tools=disabled \
        -Dshader-cache=disabled \
        -Dxmlconfig=disabled \
        -Dbuild-tests=false
fi

# The WebGPU driver too, when this tree has been configured for it (an
# emdawnwebgpu package and a Tint build -- see build-mesa-wasm.sh). Same Mesa,
# same decoder, a different Gallium driver under it, which is exactly what the
# session's "OpenGL through:" choice switches between.
TARGETS=src/gallium/targets/gex-wasm/gex-virgl-replay.js
if meson configure "$BUILD" 2>/dev/null | grep -q '^  gex-emdawn-dir *[^ ]'; then
    TARGETS="$TARGETS src/gallium/targets/gex-wasm/gex-wgpu-replay.js"
fi

# shellcheck disable=SC2086
ninja -C "$BUILD" $TARGETS -j"${JOBS:-$(nproc)}"

mkdir -p "$PUB"
for f in gex-virgl-replay.js gex-virgl-replay.wasm gex-wgpu-replay.js gex-wgpu-replay.wasm; do
    [ -f "$BUILD/src/gallium/targets/gex-wasm/$f" ] || continue
    cp "$BUILD/src/gallium/targets/gex-wasm/$f" "$PUB/$f"
    echo "  $PUB/$f: $(du -h "$PUB/$f" | cut -f1)"
done
