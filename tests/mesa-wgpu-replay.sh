#!/bin/sh
# Replay a recorded guest GL stream through Mesa + gex_wgpu on real WebGPU.
#
#   tests/mesa-wgpu-replay.sh [TRACE] [es|core]
#
# The production path end to end, in a browser: guest wire bytes -> gex's
# decoder -> st/mesa -> NIR -> the gex_wgpu Gallium driver -> WebGPU. The same
# trace run through tests/gl-replay.mjs goes to softpipe instead, and both
# print an FNV digest of the final surface, so the two backends are compared by
# value rather than by eye.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}
BUILD=$OUT/build/src/gallium/targets/gex-wasm
TRACE=${1:-$ROOT/tests/gl-traces/gles-shader-compile.gexgltr}
API=${2:-es}
# Optional: ask st/mesa for a specific GL version. Zero means the backend's
# own default for the selected API.
GLMAJOR=${3:-0}
GLMINOR=${4:-0}
REPLAY=$BUILD/gex-mesa-replay.js

[ -f "$BUILD/gex-wgpu-replay.wasm" ] || {
    echo "no gex-wgpu-replay; run web/build-mesa-wasm.sh (needs GEX_MESA_EMDAWN)" >&2
    exit 1
}
[ -f "$TRACE" ] || { echo "no trace at $TRACE" >&2; exit 1; }

for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    mkdir -p "$tree/gex"
    cp "$BUILD/gex-wgpu-replay.js" "$BUILD/gex-wgpu-replay.wasm" "$tree/gex/"
    cp "$TRACE" "$tree/gex/replay.gexgltr"
done
cp "$ROOT/web/public/mesa-wgpu-replay.html" "$ROOT/web/dist/mesa-wgpu-replay.html"

WORK=${TMPDIR:-/tmp}/gex-mesa-wgpu-replay
mkdir -p "$WORK"

# Softpipe first: the same file, the same metrics, a backend that is not under
# test. Without it a WebGPU number has nothing to mean.
echo "---- softpipe (reference) ----"
node "$ROOT/tests/gl-replay.mjs" "$TRACE" "$REPLAY" --api "$API" \
    --gl-major "$GLMAJOR" --gl-minor "$GLMINOR" --readback \
    --dump-surface "$WORK/softpipe.gexsurf" >"$WORK/softpipe.log" 2>&1 || {
        echo "FAIL mesa-wgpu-replay: the reference backend could not replay this trace"
        grep -v "Heap resize" "$WORK/softpipe.log" | tail -10 | sed 's/^/     /'
        exit 1
    }
grep -e "^REPLAY" -e "^PASS" "$WORK/softpipe.log" | sed 's/^/     /'

echo "---- Mesa + gex_wgpu on WebGPU ----"
GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
GEX_WGPU_PAGE=${GEX_WGPU_PAGE:-mesa-wgpu-replay.html} \
GEX_WGPU_QUERY="trace=/gex/replay.gexgltr&api=$API&major=$GLMAJOR&minor=$GLMINOR" \
GEX_WGPU_TIMEOUT_MS=${GEX_WGPU_TIMEOUT_MS:-300000} \
GEX_WGPU_SURFACE_OUT="$WORK/wgpu.gexsurf" \
    node "$ROOT/tests/mesa-wgpu-browser.mjs" >"$WORK/wgpu.log" 2>&1 || {
        echo "FAIL mesa-wgpu-replay: WebGPU replay failed"
        grep -v "Heap resize" "$WORK/wgpu.log" | tail -12 | sed 's/^/     /'
        exit 1
    }
grep -e "^PASS gex-wgpu-browser" "$WORK/wgpu.log" | sed 's/^/     /'

# The pictures, pixel by pixel.
#
# A mean was the comparison here for a correct reason -- an exact digest cannot
# hold a CPU rasteriser and a GPU to the same bits -- but it is blind to almost
# everything worth catching. A vertically flipped image has exactly the same
# mean and exactly the same coverage, and rendering SuperTuxKart's scene upside
# down is a defect this project actually shipped.
#
# tools/surface-diff.py gets the strictness back: legitimate disagreement lives
# on primitive edges, so it erodes the difference mask and counts only what
# survives. On this trace the two backends differ in three pixels and none of
# them survive. The mean check is kept below as a cheap second opinion.
if [ -f "$WORK/softpipe.gexsurf" ] && [ -f "$WORK/wgpu.gexsurf" ]; then
    python3 "$ROOT/tools/surface-diff.py" \
        "$WORK/softpipe.gexsurf" "$WORK/wgpu.gexsurf" \
        --label "$(basename "$TRACE")" \
        --tolerance "${GEX_WGPU_PIXEL_TOLERANCE:-8}" \
        --erode "${GEX_WGPU_ERODE:-1}" \
        --max-pixels "${GEX_WGPU_MAX_INTERIOR:-0}" | sed 's/^/     /' || {
        echo "FAIL mesa-wgpu-replay: the two backends do not render the same picture"
        exit 1
    }
else
    echo "     note: no surface pair to compare; falling back to the mean alone"
fi

# The mean as well: it is nearly free, and it catches a whole-surface shift
# that erosion would trim away at the border.
TOLERANCE=${GEX_WGPU_MEAN_TOLERANCE:-4}
node -e '
const { readFileSync } = require("node:fs");
const pick = (text, re) => { const m = text.match(re); return m ? m[1] : null; };
const soft = pick(readFileSync(process.argv[1], "utf8"), /^REPLAY .*mean=([0-9.,]+)/m);
const wgpu = pick(readFileSync(process.argv[2], "utf8"), /"mean":"([0-9.,]+)"/);
const tol = Number(process.argv[3]);
if (!soft || !wgpu) {
  console.error(`FAIL mesa-wgpu-replay: no mean from ${!soft ? "softpipe" : "WebGPU"}`);
  process.exit(1);
}
const a = soft.split(",").map(Number), b = wgpu.split(",").map(Number);
const worst = Math.max(...a.map((v, i) => Math.abs(v - b[i])));
console.log(`     softpipe mean ${soft}`);
console.log(`     webgpu   mean ${wgpu}`);
console.log(`     worst channel difference ${worst.toFixed(2)} (tolerance ${tol})`);
if (worst > tol) {
  console.error("FAIL mesa-wgpu-replay: the two backends disagree beyond tolerance");
  process.exit(1);
}
' "$WORK/softpipe.log" "$WORK/wgpu.log" "$TOLERANCE"

echo "ok   mesa-wgpu-replay"
