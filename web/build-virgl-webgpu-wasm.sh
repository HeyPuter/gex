#!/bin/sh
# Build the compatibility renderer as:
# guest Mesa virgl -> wasm virglrenderer -> MobileGL -> browser WebGPU.
set -eu

OUT="${1:-${TMPDIR:-/tmp}/gex-virgl-wasm}"
ROOT="$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"
MOBILEGL="$ROOT/third_party/mobilegl-webgpu"
VIRGL="$OUT/virglrenderer"
VIRGL_BUILD="$OUT/build"
MOBILEGL_BUILD="$OUT/mobilegl-webgpu-build"
TINT_OUT="$OUT/mobilegl-tint"
TINT_TMP="$OUT/mobilegl-tint-build"

command -v emcmake >/dev/null 2>&1 || { echo "need emscripten (emcmake)"; exit 1; }
[ -f "$MOBILEGL/CMakeLists.txt" ] || {
    echo "initialize MobileGL: git submodule update --init --recursive third_party/mobilegl-webgpu"
    exit 1
}
if [ ! -f "$MOBILEGL/3rdparty/glslang/External/spirv-tools/CMakeLists.txt" ]; then
    (cd "$MOBILEGL/3rdparty/glslang" && ./update_glslang_sources.py)
fi

# Reuse the existing virgl build and its browser-specific vtest transport
# patches. This also leaves the proven WebGL fallback beside the WebGPU build.
if [ ! -f "$VIRGL_BUILD/src/libvirglrenderer.a" ]; then
    "$ROOT/web/build-virgl-wasm.sh" "$OUT"
fi

# The fork pins a 2025 Tint. Current Clang diagnoses its use of __COUNTER__
# under -pedantic-errors unless this compatibility warning is disabled.
if [ ! -f "$TINT_OUT/libtint.a" ]; then
    CXXFLAGS="${CXXFLAGS:-} -Wno-c2y-extensions -Wno-deprecated-pragma" \
        OUTPUT_DIR="$TINT_OUT" TMP_DIR="$TINT_TMP" KEEP_TMP=true \
        JOBS="${JOBS:-$(nproc)}" "$MOBILEGL/tools/build-tint.sh"
fi

VTEST_FLAGS="$(cd "$VIRGL_BUILD" && python3 -c "
import json, shlex
for entry in json.load(open('compile_commands.json')):
    if entry['file'].endswith('vtest_renderer.c'):
        args = shlex.split(entry['command'])
        print(' '.join(a if not (a.startswith('-I') and not a[2:].startswith('/'))
                       else '-I$VIRGL_BUILD/' + a[2:]
                       for a in args if a.startswith(('-I', '-D'))))
        break
")"

emcmake cmake -S "$MOBILEGL" -B "$MOBILEGL_BUILD" \
    -DCMAKE_BUILD_TYPE=Release \
    -DMOBILEGL_BUILD_TEST=OFF -DMOBILEGL_BUILD_BENCHMARK=OFF \
    -DMOBILEGL_FORCE_RELEASE_OPT=OFF -DMOBILEGL_POSITION_INDEPENDENT_CODE=OFF \
    -DMOBILEGL_TINT_DIR="$TINT_OUT" -DMOBILEGL_BUILD_GEX_VIRGL=ON \
    -DGEX_VIRGL_SOURCE="$ROOT/web/virgl/gexvirgl.c" \
    -DGEX_VTEST_RENDERER="$VIRGL/vtest/vtest_renderer.c" \
    -DGEX_VTEST_UTIL="$VIRGL/vtest/util.c" \
    -DGEX_VTEST_SHM="$VIRGL/vtest/vtest_shm.c" \
    -DGEX_EPOXY_SOURCE="$ROOT/web/virgl/epoxy-desktop.c" \
    -DGEX_EPOXY_INCLUDE="$OUT/epoxy/include" \
    -DGEX_VIRGL_ARCHIVE="$VIRGL_BUILD/src/libvirglrenderer.a" \
    -DGEX_VIRGL_CORE_ARCHIVE="$VIRGL_BUILD/src/libvirgl.a" \
    -DGEX_GALLIUM_ARCHIVE="$VIRGL_BUILD/src/gallium/libgallium.a" \
    -DGEX_MESA_ARCHIVE="$VIRGL_BUILD/src/mesa/libmesa.a" \
    -DGEX_EPOXY_ARCHIVE="$OUT/epoxy/lib/libepoxy.a" \
    -DGEX_VTEST_FLAGS="$VTEST_FLAGS"
cmake --build "$MOBILEGL_BUILD" --target gexvirgl-webgpu -j"${JOBS:-$(nproc)}"
cmake -E copy "$MOBILEGL_BUILD/gexvirgl-webgpu.mjs" "$OUT/gexvirgl-webgpu.mjs"
cmake -E copy "$MOBILEGL_BUILD/gexvirgl-webgpu.wasm" "$OUT/gexvirgl-webgpu.wasm"
echo "built $OUT/gexvirgl-webgpu.mjs and .wasm"
