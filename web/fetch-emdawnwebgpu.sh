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
    PKG="$OUT/emdawnwebgpu_pkg"
elif [ -f "$OUT/webgpu/include/webgpu/webgpu.h" ]; then
    PKG="$OUT"
else
    echo "unexpected emdawnwebgpu archive layout in $OUT" >&2
    exit 1
fi

# Upstream typo in the GPUBlendFactor table: the two dual-source *alpha*
# factors are spelled without the hyphen the WebGPU IDL requires, so the
# browser throws
#
#   TypeError: The provided value 'src1alpha' is not a valid enum value
#   of type GPUBlendFactor
#
# out of wgpuDeviceCreateRenderPipeline, and the draw is lost. Native Dawn
# takes the C enum and never builds a string, so dual-source blending passes
# every native gate and fails only in a browser -- which is where it matters,
# since subpixel text rendering is what asks for it.
#
# Patched here rather than in a build step because the unzip above is skipped
# when the tree already exists: a fix applied anywhere else is undone by the
# next fetch into a clean .work, silently. Idempotent, and it checks that the
# broken spelling is still there so a fixed upstream does not fail the build.
ENUMS="$PKG/webgpu/src/library_webgpu_enum_tables.js"
if [ -f "$ENUMS" ] && grep -q "'src1alpha'" "$ENUMS"; then
    sed -i "s/'src1alpha'/'src1-alpha'/; s/'one-minus-src1alpha'/'one-minus-src1-alpha'/" "$ENUMS"
fi

printf '%s\n' "$PKG"
