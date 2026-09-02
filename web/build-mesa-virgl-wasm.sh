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
MESA=$ROOT/third_party/mesa
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
# `meson configure`'s table has a value column that is hard to grep -- an
# empty value still leaves non-blank text on the line -- so ask introspect.
if meson introspect "$BUILD" --buildoptions 2>/dev/null | python3 -c '
import json, sys
o = [x for x in json.load(sys.stdin) if x["name"] == "gex-emdawn-dir"]
sys.exit(0 if o and o[0]["value"] else 1)'; then
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
