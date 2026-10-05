#!/bin/sh
# Point-parameter state, vector marshalling, error, and canary differential gate.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
SOURCE="$ROOT/tests/integration/programs/gl44-point-parameter-probe.c"
WORK=${GEX_POINT_PARAMETER_WORK:-${TMPDIR:-/tmp}/gex-gl44-point-parameter}
GUEST_CC=${GEX_CC:-$HOME/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc}
mkdir -p "$WORK" "$ROOT/web/public/fs" "$ROOT/web/dist/fs"

${CC:-cc} -O2 -Wall -Wextra -o "$WORK/mesa" "$SOURCE" -lEGL -lGL -lm
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 "$WORK/mesa" | tee "$WORK/mesa.log"
grep -q 'SUMMARY cases=10 passed=10 failed=0' "$WORK/mesa.log"

GEX_CC="$GUEST_CC" "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
"$GUEST_CC" -O2 -Wall -Wextra -I"$ROOT/third_party/mesa/include" \
    -o "$WORK/probe" "$SOURCE" "$WORK/guest-gl/libGLESv2.so.2" -lm
for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    cp "$WORK/probe" "$tree/fs/gl44-point-parameter-probe"
    cp "$WORK/guest-gl/libGLESv2.so.2" "$tree/fs/libGLESv2.so.2"
    cp "$WORK/guest-gl/libEGL.so.1" "$tree/fs/libEGL.so.1"
done

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
# Which host backend decodes the guest's GL: `gl=mesa` is st/mesa on the
# gex_wgpu Gallium driver. These probes check
# their own results against native Mesa first, so they are the only
# self-checking ground truth this project has for the Mesa path.
node "$ROOT/web/test/drive.mjs" gears --allow-errors \
    --query "${GEX_GL_QUERY:-gl=mesa}" \
    --query glclient=/fs/gl44-point-parameter-probe \
    --want 'SUMMARY cases=10 passed=10 failed=0' --fail 'CASE .* FAIL|FATAL' \
    --wall 120 --idle 60
