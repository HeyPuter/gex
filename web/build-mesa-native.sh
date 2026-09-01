#!/bin/sh
# Build Mesa with the gex_wgpu Gallium driver for the desktop, against native
# Dawn.
#
# The point is to put the driver under real Linux applications without the
# guest, the browser, or the command ring in the way: an ordinary GLX program
# loads this libGL, Mesa picks gex_wgpu, and the driver talks to Dawn's native
# WebGPU. The same driver source the browser build uses -- it never depended
# on emscripten, only on webgpu.h.
#
# softpipe is built alongside deliberately. It is the reference the same
# application can be run against by changing one environment variable, which
# is the only way to tell "the driver is wrong" from "the application asked
# for something odd".
#
#   GALLIUM_DRIVER=gex_wgpu LIBGL_ALWAYS_SOFTWARE=1 \
#   LIBGL_DRIVERS_PATH=<out>/lib/dri LD_LIBRARY_PATH=<out>/lib <program>
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${GEX_MESA_NATIVE_OUT:-${TMPDIR:-/tmp}/gex-mesa-native}
DAWN=${GEX_DAWN_PKG:-$HOME/.cache/gex-dawn-native/pkg}
EMDAWN=${GEX_EMDAWN_DIR:-$ROOT/web/.work/emdawnwebgpu-v20250428.134257/emdawnwebgpu_pkg}

[ -f "$DAWN/lib/libtint.a" ] || {
    echo "no $DAWN/lib/libtint.a -- run ~/.cache/gex-dawn-native/build.sh then package.sh" >&2
    exit 1
}
# The WebGPU headers come from the emdawnwebgpu package rather than from the
# Dawn build: they are the same revision, and the one emscripten-only
# declaration in them sits behind #ifdef __EMSCRIPTEN__.
[ -d "$EMDAWN/webgpu/include" ] || {
    echo "no WebGPU headers at $EMDAWN -- run web/fetch-emdawnwebgpu.sh" >&2
    exit 1
}

if [ ! -f "$OUT/build.ninja" ]; then
    meson setup "$OUT" "$ROOT/third_party/mesa" \
        --buildtype=release \
        -Dgallium-drivers=gex_wgpu,softpipe \
        -Dvulkan-drivers= \
        -Dglx=dri -Degl=enabled -Dgbm=disabled -Dplatforms=x11 \
        -Dllvm=disabled -Dshared-llvm=disabled \
        -Dgles1=disabled -Dgles2=enabled \
        -Dgex-emdawn-dir="$EMDAWN" \
        -Dgex-tint-dir="$DAWN" \
        -Dgex-dawn-native-dir="$DAWN/lib" \
        -Dbuild-tests=false
fi
ninja -C "$OUT"
echo "built: $OUT"
find "$OUT" -name "libgallium_dri.so" -o -name "libGL.so*" | head -5
