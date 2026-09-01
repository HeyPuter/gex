#!/bin/sh
# Build the direct guest GLES API -> MobileGL -> browser WebGPU renderer.
set -eu

OUT="${1:-${TMPDIR:-/tmp}/gex-mobilegl-wasm}"
ROOT="$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"
MOBILEGL="$ROOT/third_party/mobilegl-webgpu"
BUILD="$OUT/build"
TINT_OUT="${GEX_MOBILEGL_TINT:-${TMPDIR:-/tmp}/gex-virgl-wasm/mobilegl-tint}"
TINT_TMP="$OUT/tint-build"

command -v emcmake >/dev/null 2>&1 || { echo "need emscripten (emcmake)"; exit 1; }
[ -f "$MOBILEGL/CMakeLists.txt" ] || { echo "initialize third_party/mobilegl-webgpu"; exit 1; }

if [ ! -f "$MOBILEGL/3rdparty/glslang/External/spirv-tools/CMakeLists.txt" ]; then
    (cd "$MOBILEGL/3rdparty/glslang" && ./update_glslang_sources.py)
fi
if [ ! -f "$TINT_OUT/libtint.a" ]; then
    CXXFLAGS="${CXXFLAGS:-} -Wno-c2y-extensions -Wno-deprecated-pragma" \
        OUTPUT_DIR="$TINT_OUT" TMP_DIR="$TINT_TMP" KEEP_TMP=true \
        JOBS="${JOBS:-$(nproc)}" "$MOBILEGL/tools/build-tint.sh"
fi

emcmake cmake -S "$MOBILEGL" -B "$BUILD" \
    -DCMAKE_BUILD_TYPE=Release \
    -DMOBILEGL_BUILD_TEST=OFF -DMOBILEGL_BUILD_BENCHMARK=OFF \
    -DMOBILEGL_FORCE_RELEASE_OPT=OFF -DMOBILEGL_ENABLE_LTO=ON \
    -DMOBILEGL_POSITION_INDEPENDENT_CODE=OFF \
    -DMOBILEGL_TINT_DIR="$TINT_OUT" -DMOBILEGL_BUILD_GEX_GL=ON \
    -DGEX_GL_SOURCE="$ROOT/web/mobilegl/gexgl-mobilegl.cpp"
cmake --build "$BUILD" --target gexmobilegl-webgpu -j"${JOBS:-$(nproc)}"
mkdir -p "$OUT"
cp "$BUILD/gexmobilegl-webgpu.mjs" "$BUILD/gexmobilegl-webgpu.wasm" "$OUT/"
echo "$OUT/gexmobilegl-webgpu.{mjs,wasm}"
