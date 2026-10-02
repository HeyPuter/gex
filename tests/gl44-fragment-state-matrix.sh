#!/bin/sh
# Differential fragment-state semantics gate: establish the oracle with Mesa,
# then run the exact same source through the persistent browser WebGPU path.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
SOURCE="$ROOT/tests/integration/programs/gl44-fragment-state-matrix-probe.c"
WORK=${GEX_FRAGMENT_STATE_WORK:-${TMPDIR:-/tmp}/gex-gl44-fragment-state-matrix}
NATIVE_CC=${CC:-cc}
GUEST_CC=${GEX_CC:-}

mkdir -p "$WORK" "$ROOT/web/public/fs" "$ROOT/web/dist/fs"

if [ -z "$GUEST_CC" ]; then
    for candidate in \
        /home/velzie/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc \
        /usr/bin/aarch64-linux-musl-gcc \
        /usr/bin/aarch64-linux-gnu-gcc; do
        if [ -x "$candidate" ]; then
            GUEST_CC=$candidate
            break
        fi
    done
fi
[ -n "$GUEST_CC" ] || {
    echo "no aarch64 C compiler; set GEX_CC" >&2
    exit 2
}

"$NATIVE_CC" -O2 -Wall -Wextra -o "$WORK/mesa" "$SOURCE" -lEGL -lGL
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 "$WORK/mesa" | tee "$WORK/mesa.log"
grep -q 'SUMMARY cases=23 passed=23 failed=0' "$WORK/mesa.log"

GEX_CC="$GUEST_CC" "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
"$GUEST_CC" -O2 -Wall -Wextra \
    -I"$ROOT/third_party/mesa/include" \
    -o "$WORK/gl44-fragment-state-matrix-probe" "$SOURCE" \
    "$WORK/guest-gl/libGLESv2.so.2"

for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    cp "$WORK/gl44-fragment-state-matrix-probe" "$tree/fs/gl44-fragment-state-matrix-probe"
    cp "$WORK/guest-gl/libGLESv2.so.2" "$tree/fs/libGLESv2.so.2"
    cp "$WORK/guest-gl/libEGL.so.1" "$tree/fs/libEGL.so.1"
done

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
# Which host backend decodes the guest's GL: `gl=mesa` is st/mesa on the
# gex_wgpu Gallium driver. These probes check
# their own results against native Mesa, so they are the only ground truth
# this project has for the Mesa path -- SuperTuxKart cannot provide one.
GEX_GL_QUERY=${GEX_GL_QUERY:-gl=mesa} \
node "$ROOT/web/test/drive.mjs" gears \
    --allow-errors \
    --query "${GEX_GL_QUERY:-gl=mesa}" \
    --query glclient=/fs/gl44-fragment-state-matrix-probe \
    --want 'SUMMARY cases=23 passed=23 failed=0' \
    --fail 'CASE .* FAIL|FATAL' \
    --wall 120 --idle 60
