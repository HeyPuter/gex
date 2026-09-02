#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
SOURCE="$ROOT/tests/integration/programs/gl44-transform-feedback-probe.c"
WORK=${GEX_TRANSFORM_FEEDBACK_WORK:-${TMPDIR:-/tmp}/gex-gl44-transform-feedback}
GUEST_CC=${GEX_CC:-/home/velzie/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc}
mkdir -p "$WORK" "$ROOT/web/public/fs" "$ROOT/web/dist/fs"

${CC:-cc} -O2 -Wall -Wextra -o "$WORK/mesa" "$SOURCE" -lEGL -lGL
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 "$WORK/mesa" | tee "$WORK/mesa.log"
grep -q 'SUMMARY cases=1 passed=1 failed=0' "$WORK/mesa.log"

GEX_CC="$GUEST_CC" "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
"$GUEST_CC" -O2 -Wall -Wextra -I"$ROOT/third_party/mobilegl-webgpu/include" \
    -o "$WORK/probe" "$SOURCE" "$WORK/guest-gl/libGLESv2.so.2"
for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    cp "$WORK/probe" "$tree/fs/gl44-transform-feedback-probe"
    cp "$WORK/guest-gl/libGLESv2.so.2" "$tree/fs/libGLESv2.so.2"
    cp "$WORK/guest-gl/libEGL.so.1" "$tree/fs/libEGL.so.1"
done

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
# Which host backend decodes the guest's GL: `webgpu=1` is MobileGL,
# `gl=mesa` is st/mesa on the gex_wgpu Gallium driver. These probes check
# their own results against native Mesa first, so they are the only
# self-checking ground truth this project has for the Mesa path.
node "$ROOT/web/test/drive.mjs" gears --allow-errors \
    --query "${GEX_GL_QUERY:-gl=mesa}" \
    --query glclient=/fs/gl44-transform-feedback-probe \
    --want 'SUMMARY cases=1 passed=1 failed=0' --fail 'SUMMARY .*failed=1|FATAL' \
    --wall 120 --idle 60
