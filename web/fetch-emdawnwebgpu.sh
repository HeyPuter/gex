#!/bin/sh
# Fetch the standardized Dawn WebGPU C bindings used by the Mesa/Wasm driver.
set -eu

VERSION=v20250428.134257
SHA512=e8eb56157f52c229e87848a6c9cd062035a987ee420cfe05287dc7b7d6bd0cae1cdcd8d010ad362f40f6a785b3133ca2efe0e655c94f5b3d92210fb0e59c574e
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/web/.work/emdawnwebgpu-$VERSION}
ARCHIVE=$OUT/emdawnwebgpu.zip

command -v curl >/dev/null 2>&1 || { echo "need curl" >&2; exit 1; }
command -v unzip >/dev/null 2>&1 || { echo "need unzip" >&2; exit 1; }
mkdir -p "$OUT"

if [ ! -f "$ARCHIVE" ]; then
    curl --fail --location --retry 3 \
        "https://github.com/google/dawn/releases/download/$VERSION/emdawnwebgpu_pkg-$VERSION.zip" \
        --output "$ARCHIVE"
fi
printf '%s  %s\n' "$SHA512" "$ARCHIVE" | sha512sum --check --status || {
    echo "emdawnwebgpu archive checksum mismatch: $ARCHIVE" >&2
    exit 1
}

if [ ! -f "$OUT/emdawnwebgpu_pkg/webgpu/include/webgpu/webgpu.h" ] &&
   [ ! -f "$OUT/webgpu/include/webgpu/webgpu.h" ]; then
    unzip -q "$ARCHIVE" -d "$OUT"
fi

if [ -f "$OUT/emdawnwebgpu_pkg/webgpu/include/webgpu/webgpu.h" ]; then
    printf '%s\n' "$OUT/emdawnwebgpu_pkg"
elif [ -f "$OUT/webgpu/include/webgpu/webgpu.h" ]; then
    printf '%s\n' "$OUT"
else
    echo "unexpected emdawnwebgpu archive layout in $OUT" >&2
    exit 1
fi
