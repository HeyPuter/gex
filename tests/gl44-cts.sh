#!/bin/sh
# Build the pinned Khronos OpenGL CTS for the aarch64 guest. Running cases in
# the browser is a separate, explicit step because even a smoke case translates
# a large C++ executable and is intentionally much slower than the unit probes.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
CTS="$ROOT/third_party/vk-gl-cts"
PATCH="$ROOT/tests/cts/vk-gl-cts-gex.patch"
BUILD=${GEX_CTS_BUILD:-$ROOT/web/.work/gl44-cts}
PIN=067e8832315e79817ede1c4863804e440f5d1c80

: "${GEX_CTS_TOOLCHAIN:?set GEX_CTS_TOOLCHAIN to the aarch64-linux-musl toolchain root}"

if [ ! -e "$CTS/.git" ]; then
    git -C "$ROOT" submodule update --init --depth 1 third_party/vk-gl-cts
fi
actual=$(git -C "$CTS" rev-parse HEAD)
[ "$actual" = "$PIN" ] || {
    echo "CTS revision $actual does not match pinned $PIN" >&2
    exit 1
}

# The overlay only teaches upstream's generic surfaceless EGL platform how to
# select a desktop-GL config and where this repository's EGL headers live.
applied=false
cleanup() {
    if [ "$applied" = true ]; then
        git -C "$CTS" apply --reverse "$PATCH"
    fi
}
trap cleanup EXIT INT TERM
if git -C "$CTS" apply --reverse --check "$PATCH" 2>/dev/null; then
    : # already applied
else
    git -C "$CTS" apply --check "$PATCH"
    git -C "$CTS" apply "$PATCH"
    applied=true
fi

if [ ! -f "$CTS/external/zlib/src/zlib.h" ]; then
    (cd "$CTS" && python3 external/fetch_sources.py)
fi

GUEST_GL="$BUILD/guest-gl"
GEX_CC="$GEX_CTS_TOOLCHAIN/bin/aarch64-linux-musl-gcc" \
    "$ROOT/guest/gl/build.sh" "$GUEST_GL"

cmake -S "$CTS" -B "$BUILD" \
    -DCMAKE_TOOLCHAIN_FILE="$ROOT/tests/cts/aarch64-musl.cmake" \
    -DDEQP_TARGET=gex \
    -DGEX_GUEST_GL_LIBRARY="$GUEST_GL/libGLESv2.so.2" \
    -DCMAKE_SKIP_RPATH=ON \
    -DSELECTED_BUILD_TARGETS=glcts \
    -DDEQP_DISABLE_VK_VIDEO_TESTS=ON \
    -DCMAKE_BUILD_TYPE=Release
cmake --build "$BUILD" --target glcts -j"${GEX_CTS_JOBS:-4}"

"$GEX_CTS_TOOLCHAIN/bin/aarch64-linux-musl-gcc" -O2 \
    "$ROOT/tests/cts/glcts-log-wrapper.c" \
    -o "$BUILD/glcts-log-wrapper"

binary=$(find "$BUILD" -type f -name glcts -perm -0100 | head -n 1)
[ -n "$binary" ] || { echo "glcts build produced no executable" >&2; exit 1; }
echo "$binary"
