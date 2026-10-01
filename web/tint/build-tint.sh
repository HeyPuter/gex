#!/usr/bin/env bash
# Build Dawn's Tint for wasm32: the SPIR-V reader and WGSL writer that the
# gex_wgpu Gallium driver links (Mesa's `-Dgex-tint-dir`). Called by
# web/build-mesa-wasm.sh when its Tint directory has no libtint.a yet.
#
# Output, in OUTPUT_DIR:
#   libtint.a   Tint, Abseil and SPIRV-Tools objects in one archive
#   include/    headers, rooted for "src/tint/..." and "absl/..."
#   tint.flags  what a consumer has to compile and link with
#
# Environment:
#   OUTPUT_DIR    where the output goes (default: $TMPDIR/gex-tint)
#   TMP_DIR       Dawn checkout and build directory (default: a fresh mktemp)
#   KEEP_TMP      true keeps TMP_DIR afterwards, for an incremental rebuild
#   WASM_THREADS  true builds with -pthread, for a shared-memory module
#   JOBS          build parallelism
#
# Needs emcmake, emcc and emar (emsdk), git and python3. Nothing from Dawn is
# kept in this tree; only this script and dawn-tint-emscripten.patch.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

# The Dawn revision matches the emdawnwebgpu package (web/fetch-emdawnwebgpu.sh),
# so the WGSL Tint writes is the WGSL those bindings expect.
DAWN_REPO="${DAWN_REPO:-https://dawn.googlesource.com/dawn}"
DAWN_REF="${DAWN_REF:-b465e23d19a5e27402d80191c0f8de1fb3b385b5}"
DAWN_PATCH="${DAWN_PATCH:-$HERE/dawn-tint-emscripten.patch}"
# SPIRV-Tools and SPIRV-Headers are pinned rather than taken from Dawn's DEPS.
# These are the revisions every libtint.a used by gex so far was built with.
SPIRV_TOOLS_REV="${SPIRV_TOOLS_REV:-33e02568181e3312f49a3cf33df470bf96ef293a}"
SPIRV_HEADERS_REV="${SPIRV_HEADERS_REV:-2a611a970fdbc41ac2e3e328802aed9985352dca}"
OUTPUT_DIR="${OUTPUT_DIR:-${TMPDIR:-/tmp}/gex-tint}"
TMP_DIR="${TMP_DIR:-}"
KEEP_TMP="${KEEP_TMP:-false}"
WASM_THREADS="${WASM_THREADS:-false}"
JOBS="${JOBS:-$(nproc)}"

say() { echo "[build-tint] $*"; }
for tool in emcmake emcc emar git python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "need $tool" >&2; exit 1; }
done

[ -n "$TMP_DIR" ] || TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/gex-tint-build.XXXXXX")"
DAWN_SRC="$TMP_DIR/dawn"
DAWN_BUILD="$TMP_DIR/dawn-build"
cleanup() { [ "$KEEP_TMP" = true ] || rm -rf "$TMP_DIR"; }
trap cleanup EXIT

# A shallow checkout of exactly one revision.
checkout() {  # <dir> <url> <rev>
    rm -rf "$1"
    mkdir -p "$1"
    git -C "$1" init -q
    git -C "$1" fetch -q --depth 1 "$2" "$3"
    git -C "$1" checkout -q FETCH_HEAD
}

say "Dawn $DAWN_REF"
checkout "$DAWN_SRC" "$DAWN_REPO" "$DAWN_REF"
git -C "$DAWN_SRC" apply "$DAWN_PATCH"
python3 "$DAWN_SRC/tools/fetch_dawn_dependencies.py" -d "$DAWN_SRC" -s >/dev/null

# fetch_dawn_dependencies leaves the dependencies without a .git of their own,
# so the pins replace them with fresh checkouts. Dawn looks for the sources in
# <dep>/src.
say "SPIRV-Tools $SPIRV_TOOLS_REV, SPIRV-Headers $SPIRV_HEADERS_REV"
checkout "$DAWN_SRC/third_party/spirv-tools/src" \
    https://github.com/KhronosGroup/SPIRV-Tools "$SPIRV_TOOLS_REV"
checkout "$DAWN_SRC/third_party/spirv-headers/src" \
    https://github.com/KhronosGroup/SPIRV-Headers "$SPIRV_HEADERS_REV"

# Tint alone: every Dawn backend off, the SPIR-V reader and WGSL reader and
# writer on. Current Clang rejects the pinned Tint's __COUNTER__ use under
# -pedantic-errors without the first warning flag.
CXX_WARNINGS='-Wno-c2y-extensions -Wno-deprecated-pragma -Wno-lifetime-safety-intra-tu-suggestions'
if [ "$WASM_THREADS" = true ]; then
    FLAG_ARGS=(-DCMAKE_C_FLAGS=-pthread "-DCMAKE_CXX_FLAGS=-pthread $CXX_WARNINGS")
else
    FLAG_ARGS=("-DCMAKE_CXX_FLAGS=$CXX_WARNINGS")
fi
say "configuring"
emcmake cmake -S "$DAWN_SRC" -B "$DAWN_BUILD" -DCMAKE_BUILD_TYPE=Release \
    -DDAWN_ENABLE_VULKAN=OFF -DDAWN_ENABLE_D3D11=OFF -DDAWN_ENABLE_D3D12=OFF \
    -DDAWN_ENABLE_METAL=OFF -DDAWN_ENABLE_OPENGLES=OFF -DDAWN_ENABLE_DESKTOP_GL=OFF \
    -DDAWN_ENABLE_NULL=OFF -DDAWN_ENABLE_SWIFTSHADER=OFF \
    -DDAWN_BUILD_SAMPLES=OFF -DDAWN_BUILD_TESTS=OFF -DDAWN_BUILD_BENCHMARKS=OFF \
    -DTINT_BUILD_TESTS=OFF -DTINT_BUILD_CMD_TOOLS=OFF \
    -DTINT_BUILD_SPV_READER=ON -DTINT_BUILD_WGSL_WRITER=ON -DTINT_BUILD_WGSL_READER=ON \
    -DTINT_BUILD_GLSL_WRITER=OFF -DTINT_BUILD_GLSL_VALIDATOR=OFF \
    -DTINT_BUILD_MSL_WRITER=OFF -DTINT_BUILD_HLSL_WRITER=OFF -DTINT_BUILD_SPV_WRITER=OFF \
    -DDAWN_USE_GLFW=OFF -DDAWN_USE_X11=OFF -DDAWN_USE_WAYLAND=OFF \
    -DDAWN_ENABLE_INSTALL=OFF -DTINT_ENABLE_INSTALL=OFF \
    "${FLAG_ARGS[@]}" >/dev/null

say "building tint_api (with Abseil and SPIRV-Tools)"
cmake --build "$DAWN_BUILD" --target tint_api -j"$JOBS" >/dev/null

rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR/include"
mapfile -t ARCHIVES < <(
    find "$DAWN_BUILD/src/tint" -name 'libtint_*.a'
    find "$DAWN_BUILD" -name 'libabsl_*.a'
    find "$DAWN_BUILD/third_party/spirv-tools" -name 'libSPIRV-Tools*.a'
)
say "merging ${#ARCHIVES[@]} archives into $OUTPUT_DIR/libtint.a"
{
    echo "create $OUTPUT_DIR/libtint.a"
    for archive in "${ARCHIVES[@]}"; do echo "addlib $archive"; done
    echo "save"
    echo "end"
} | emar -M
emar t "$OUTPUT_DIR/libtint.a" >/dev/null

( cd "$DAWN_SRC" && find src \( -name '*.h' -o -name '*.inc' \) | tar -cf - -T - ) |
    tar -xf - -C "$OUTPUT_DIR/include"
( cd "$DAWN_SRC/third_party/abseil-cpp" && find absl \( -name '*.h' -o -name '*.inc' \) | tar -cf - -T - ) |
    tar -xf - -C "$OUTPUT_DIR/include"

cat > "$OUTPUT_DIR/tint.flags" <<EOF
# Using libtint.a (Dawn $DAWN_REF):
#   compile the translation units that include Tint with
#     -std=c++20 -fno-rtti -fno-exceptions -I "$OUTPUT_DIR/include"
#     -DTINT_BUILD_SPV_READER=1 -DTINT_BUILD_WGSL_READER=1 -DTINT_BUILD_WGSL_WRITER=1
#     -DTINT_BUILD_SPV_WRITER=0 -DTINT_BUILD_GLSL_WRITER=0 -DTINT_BUILD_HLSL_WRITER=0
#     -DTINT_BUILD_MSL_WRITER=0 -DTINT_BUILD_IR_BINARY=0
#     -DTINT_BUILD_IS_LINUX=1 -DTINT_BUILD_IS_MAC=0 -DTINT_BUILD_IS_WIN=0
#   link with -sSTACK_SIZE=16MB or more: Tint's AST passes recurse deeply
#   built for a shared-memory (pthread) module: $WASM_THREADS
EOF

say "done: $OUTPUT_DIR ($(du -h "$OUTPUT_DIR/libtint.a" | cut -f1) libtint.a)"
