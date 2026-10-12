#!/bin/sh
# Report the GL version Mesa + gex_wgpu reaches on real WebGPU.
#
#   tests/mesa-wgpu-version.sh [es|core]
#
# The backend creates its context from the wire's own CONTEXT_CREATE -- only
# the guest knows which API it bound -- so this replays a one-batch trace that
# asks for the requested API and nothing else, and prints what st/mesa gave
# back along with what stands between it and the next version.
#
# It asks for version 1.0 deliberately. st_api_create_context computes the
# highest version it can supply and then only *compares* it against the
# request, so asking for the lowest reports the ceiling instead of testing a
# guess.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
API=${1:-core}
OUT=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}
BUILD=$OUT/build/src/gallium/targets/gex-wasm
WORK=${TMPDIR:-/tmp}/gex-mesa-wgpu-version

[ -f "$BUILD/gex-wgpu-replay.wasm" ] || {
    echo "no gex-wgpu-replay; run web/build-mesa-wasm.sh" >&2
    exit 1
}
mkdir -p "$WORK"
node "$ROOT/tests/gl-make-context-trace.mjs" "$WORK/context.gexgltr" "$API" 1 0 \
    >/dev/null

# The module as well as the trace. Copying only the trace measures whatever
# module happened to be published last, which is a stale answer that looks
# exactly like a real one.
for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    mkdir -p "$tree/gex"
    cp "$BUILD/gex-wgpu-replay.js" "$BUILD/gex-wgpu-replay.wasm" "$tree/gex/"
    # The cold part, when the build split one off (docs/mesa-wasm-size.md).
    rm -f "$tree/gex/gex-wgpu-replay.deferred.wasm"
    [ ! -f "$BUILD/gex-wgpu-replay.deferred.wasm" ] || cp "$BUILD/gex-wgpu-replay.deferred.wasm" "$tree/gex/"
    cp "$WORK/context.gexgltr" "$tree/gex/replay.gexgltr"
done
cp "$ROOT/web/public/mesa-wgpu-replay.html" "$ROOT/web/dist/mesa-wgpu-replay.html"

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
GEX_WGPU_PAGE=${GEX_WGPU_PAGE:-mesa-wgpu-replay.html} \
GEX_WGPU_QUERY="trace=/gex/replay.gexgltr&api=$API&major=1&minor=0" \
GEX_WGPU_TIMEOUT_MS=${GEX_WGPU_TIMEOUT_MS:-300000} \
    node "$ROOT/tests/mesa-wgpu-browser.mjs" >"$WORK/run.log" 2>&1 || true

if ! grep -q "gex-wgpu: context is" "$WORK/run.log"; then
    echo "FAIL mesa-wgpu-version: the backend never reported a context"
    grep -v "Heap resize" "$WORK/run.log" | tail -12 | sed 's/^/     /'
    exit 1
fi
grep -oE "gex-wgpu: (context is|OpenGL ES context|at or above|[0-9]+ requirement|GL [0-9.]+ wants)[^\\\\\"]*" \
    "$WORK/run.log" | sort -u | sed 's/^gex-wgpu: /     /'
