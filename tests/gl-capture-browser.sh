#!/bin/sh
# Record the guest's GL command stream in a real browser, then replay it in Node.
#
#   tests/gl-capture-browser.sh
#
# This is the leg that `tests/gl-capture-replay.sh` cannot cover: it records
# from the page's own GL device rather than the native one, which is where the
# WebGPU backend lives. A defect that only appears in the browser has to be
# recorded there before it can be reproduced anywhere else, and the assertion
# is exactly that -- what the browser emitted, Mesa can execute.
#
# Skips when there is no built page or no Mesa replay module.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
WORK=${GEX_GL_CAPTURE_WORK:-${TMPDIR:-/tmp}/gex-gl-capture-browser}
MESA_OUT=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}
REPLAY=$MESA_OUT/build/src/gallium/targets/gex-wasm/gex-mesa-replay.js

[ -f "$REPLAY" ] || { echo "skip: no Mesa replay module at $REPLAY (web/build-mesa-wasm.sh)"; exit 0; }
[ -f "$ROOT/web/dist/index.html" ] || { echo "skip: no web/dist (bash shims/browser/build.sh)"; exit 0; }

mkdir -p "$WORK"
TRACE=$WORK/gears.gexgltr
rm -f "$TRACE"

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
    node "$ROOT/web/test/drive.mjs" gears \
    --query glcapture=1 --gl-capture "$TRACE" \
    --wall 120 --idle 60 >"$WORK/drive.log" 2>&1 || {
        echo "FAIL gl-capture-browser: the gears demo did not reach its goal"
        tail -20 "$WORK/drive.log" | sed 's/^/     /'
        exit 1
    }
grep "GL capture:" "$WORK/drive.log" | sed 's/^ *//;s/^/     /'
[ -s "$TRACE" ] || { echo "FAIL gl-capture-browser: nothing was recorded"; exit 1; }

# Executed, not merely decoded: a stream the page accepted and Mesa only
# parses would prove nothing about the backend that has to run it.
node "$ROOT/tests/gl-replay.mjs" "$TRACE" "$REPLAY" --readback >"$WORK/replay.log" 2>&1 || {
    echo "FAIL gl-capture-browser: what the browser recorded, Mesa could not run"
    grep -v "Heap resize" "$WORK/replay.log" | tail -20 | sed 's/^/     /'
    exit 1
}
grep -e "^REPLAY" -e "^PASS gl-replay" "$WORK/replay.log" | sed 's/^/     /'

echo "ok   gl-capture-browser"
