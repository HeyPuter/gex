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
        --prefix "$OUT/inst" \
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
# Installed, not merely built.
#
# Every native gate -- tests/gl-cts-native.sh, the app gates, the probe
# suite -- reads $OUT/inst, and this script used to stop at ninja. The build
# directory's driver was fresh and the installed one was whatever the last
# manual `ninja install` had left, so a rebuild "succeeded" and the run under
# it measured the *old* driver. It reads as a fix that did nothing, which is
# the most expensive kind of wrong answer: the next move is to doubt the fix
# rather than the build. Checking the timestamp of $OUT/inst/lib/dri against
# the source is what caught it, and doing the install here is what stops it
# coming back.
#
# --prefix above matters for the same reason: without it a fresh setup
# defaults to /usr/local and the install silently goes somewhere no gate
# looks (or fails on permissions).
ninja -C "$OUT" install >/dev/null

# Aliases for the names a dlopen-based toolkit asks for.
#
# This build is not glvnd, so it installs a plain libGL.so.1 with the whole
# GLX implementation in it. That is enough for a program linked -lGL: the
# loader resolves the SONAME out of LD_LIBRARY_PATH and gets ours. It is not
# enough for one that dlopens GL at run time -- GLFW, SDL and Qt all do -- and
# on a glvnd system /usr/lib/libGLX.so.0 is glvnd's dispatch, which loads its
# *vendor* from /usr/lib and with it the distribution's Mesa. Two Mesa builds
# then share one process, and our libGL's call to driCreateNewScreen3 binds to
# the older one, which has no gex_wgpu driver in it:
#
#   glx: failed to create drisw screen
#
# Minecraft 26.2 fails exactly there, and reports it as "Driver does not
# support OpenGL 3.3" -- which is not what happened. GLFW 3.5 asks for
# libGLX.so.0 *before* libGL.so.1, so a file of that name next to ours, found
# first on LD_LIBRARY_PATH, is the whole fix: one Mesa in the process, and
# dlopen finds it whatever name it asks for.
if [ -d "$OUT/inst/lib" ] && [ -e "$OUT/inst/lib/libGL.so.1" ]; then
    for alias in libGLX.so.0 libOpenGL.so.0; do
        ln -sfn libGL.so.1 "$OUT/inst/lib/$alias"
    done
fi

echo "built: $OUT"
find "$OUT" -name "libgallium_dri.so" -o -name "libGL.so*" | head -5
