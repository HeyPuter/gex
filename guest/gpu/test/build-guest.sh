#!/bin/sh
# Builds gpu-smoke, a static aarch64 Linux guest that talks WebGPU through
# /dev/gex-gpu, in an Alpine container (GEX_CC overrides with a cross gcc).
#   build-guest.sh [OUTDIR]
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
GPU=$(dirname "$HERE")
ROOT=$(dirname "$(dirname "$GPU")")
OUT=${1:-$ROOT/target/gexgpu-guest}
mkdir -p "$OUT"
python3 "$ROOT/tools/gen_webgpu_wire.py" >/dev/null
SRCS="guest/gpu/wire-client.c guest/gpu/transport-dev.c guest/gpu/test/gpu-smoke.c"
FLAGS="-O2 -g -static -Wall -Wno-unused-function -Iguest/gpu -Ithird_party/dawn-api"
if [ -n "${GEX_CC:-}" ]; then
    (cd "$ROOT" && $GEX_CC $FLAGS $SRCS -o "$OUT/gpu-smoke")
else
    docker run --rm --platform linux/arm64 -v "$ROOT:/src" -v "$OUT:/out" -w /src alpine:latest \
        sh -c "apk add -q gcc musl-dev && gcc $FLAGS $SRCS -o /out/gpu-smoke"
fi
echo "$OUT/gpu-smoke"
