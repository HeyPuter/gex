#!/bin/sh
# Build the gex WebGPU wire's server for the page (guest/gpu/web/server-web.c
# and wire-server.c on emdawnwebgpu, with JSPI): web/public/gex/gexgpu-server.mjs
# and its .wasm, which gpu-bridge.mjs serves /dev/gex-gpu with.
#
# GEX_EMDAWN_PORT: emdawnwebgpu.port.py of a package matching
# third_party/dawn-api (the Dawn release its headers came from).
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
PORT=${GEX_EMDAWN_PORT:?set GEX_EMDAWN_PORT to emdawnwebgpu_pkg/emdawnwebgpu.port.py}
OUT=${1:-$ROOT/web/public/gex}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/inc" "$OUT"
# wire.h includes "webgpu.h"; here that is emdawnwebgpu's.
echo '#include <webgpu/webgpu.h>' > "$TMP/inc/webgpu.h"
python3 "$ROOT/tools/gen_webgpu_wire.py" >/dev/null
emcc -O2 -I"$TMP/inc" -I"$ROOT/guest/gpu" --use-port="$PORT" \
    -Wno-unused-function \
    "$ROOT/guest/gpu/wire-server.c" "$ROOT/guest/gpu/web/server-web.c" \
    -sJSPI -sJSPI_EXPORTS=gexgpu_web_handle,gexgpu_web_destroy \
    -sMODULARIZE -sEXPORT_ES6 -sEXPORT_NAME=createGexGpuServer \
    -sENVIRONMENT=web,worker,node -sALLOW_MEMORY_GROWTH -sINITIAL_MEMORY=33554432 \
    -sEXPORTED_FUNCTIONS=_malloc,_free -sEXPORTED_RUNTIME_METHODS=HEAPU8 \
    -o "$OUT/gexgpu-server.mjs"
echo "$OUT/gexgpu-server.mjs"
