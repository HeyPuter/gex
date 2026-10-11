#!/bin/sh
# Run a real guest GL program in the browser on Mesa + gex_wgpu + WebGPU.
#
#   tests/mesa-wgpu-live.sh [demo] [es|core] [major] [minor]
#
# This is the live path rather than a replay: an unmodified aarch64 binary
# runs under gex, its libGLESv2.so.2 fills the command ring, and the page
# executes it through gex's decoder -> st/mesa -> NIR -> gex_wgpu -> WebGPU.
# `tests/mesa-wgpu-replay.sh` proves the same backend against a recorded trace
# and compares it with softpipe; this proves a program can drive it end to end.
#
# The guest reports its own verdict -- triangles drawn, GL error, and the
# fraction of the surface covered -- which is what is asserted here. A backend
# that comes up but renders nothing still prints "covered 0%".
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
DEMO=${1:-gears}
API=${2:-es}
GLMAJOR=${3:-2}
GLMINOR=${4:-0}
WORK=${TMPDIR:-/tmp}/gex-mesa-wgpu-live
BUILD=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}/build/src/gallium/targets/gex-wasm

[ -f "$BUILD/gex-wgpu-replay.wasm" ] || {
    echo "skip: no gex-wgpu-replay; run web/build-mesa-wasm.sh" >&2
    exit 0
}
[ -f "$ROOT/web/dist/index.html" ] || {
    echo "skip: no web/dist (bash shims/browser/build.sh)" >&2
    exit 0
}

mkdir -p "$WORK" "$ROOT/web/public/gex" "$ROOT/web/dist/gex"
for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    cp "$BUILD/gex-wgpu-replay.js" "$BUILD/gex-wgpu-replay.wasm" "$tree/gex/"
    # The cold part, when the build split one off (docs/mesa-wasm-size.md).
    rm -f "$tree/gex/gex-wgpu-replay.deferred.wasm"
    [ ! -f "$BUILD/gex-wgpu-replay.deferred.wasm" ] || cp "$BUILD/gex-wgpu-replay.deferred.wasm" "$tree/gex/"
done

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
    node "$ROOT/web/test/drive.mjs" "$DEMO" \
    --query "gl=mesa" --query "glapi=$API" \
    --query "glmajor=$GLMAJOR" --query "glminor=$GLMINOR" \
    --shot "$WORK/$DEMO.png" \
    --wall 180 --idle 90 >"$WORK/drive.log" 2>&1 || {
        echo "FAIL mesa-wgpu-live: the demo did not reach its goal"
        grep -v "Heap resize" "$WORK/drive.log" | tail -20 | sed 's/^/     /'
        exit 1
    }

grep -e "^  > renderer" -e "^  > triangles" -e "^  > frames" \
     -e "^  > gl error" -e "^  > covered" "$WORK/drive.log" |
    sed 's/^  > /     /'

# The guest's own numbers are the verdict. A GL error means the program saw
# something GL-illegal; zero coverage means nothing reached the surface.
if ! grep -q "^  > gl error 0$" "$WORK/drive.log"; then
    echo "FAIL mesa-wgpu-live: the guest reported a GL error"
    grep "^  > gl error" "$WORK/drive.log" | sed 's/^  > /     /'
    exit 1
fi
if grep -q "^  > covered 0%$" "$WORK/drive.log"; then
    echo "FAIL mesa-wgpu-live: nothing was drawn to the surface"
    exit 1
fi

# Unimplemented hooks are expected while the driver is brought up, but a
# refusal at runtime means a draw was silently dropped, and the guest cannot
# see that.
if grep -q "gex-wgpu: first refusal" "$WORK/drive.log"; then
    echo "FAIL mesa-wgpu-live: the driver refused an operation during the run"
    grep -o "gex-wgpu: first refusal at [a-zA-Z_0-9:]*" "$WORK/drive.log" |
        sort -u | sed 's/^/     /'
    exit 1
fi

# And what the canvas holds, which is a different claim from "the guest drew".
#
# The guest's own `covered` figure above is a readback of what it rendered;
# it says nothing about whether any of it was copied to the page. Those are
# separate steps -- the backend copies only when the page calls display() --
# and the gears demo went without an animation loop for as long as it ran on
# Mesa, rendering perfectly into a texture nobody ever showed. Every counter
# and coverage figure in this gate was green throughout.
#
# The bounds describe "something was drawn", not this particular picture: the
# three gears measure dark=0.000 stdev=0.115 distinct=8, and a canvas nothing
# reached measures dark=0.983 stdev=0.120 distinct=2.
if command -v python3 >/dev/null 2>&1 && [ -f "$WORK/$DEMO.png" ]; then
    if python3 "$ROOT/tools/frame-stats.py" \
            --max-dark 0.60 --min-distinct 4 "$WORK/$DEMO.png" |
            sed 's/^/     /'; then
        :
    else
        echo "FAIL mesa-wgpu-live: the guest drew, but the canvas did not"
        echo "     look at $WORK/$DEMO.png"
        exit 1
    fi
fi

echo "ok   mesa-wgpu-live"
