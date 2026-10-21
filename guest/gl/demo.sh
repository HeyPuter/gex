#!/bin/sh
# Run the gears on the host's GL, from a guest that does not know it is one.
#
#   guest/gl/demo.sh [frames] [output.ppm]
#
# Builds three things and then runs the last of them:
#
#   * the guest's GL driver, libGLESv2.so.2 and libEGL.so.1, which are
#     marshallers rather than an implementation of OpenGL;
#   * a small root filesystem holding them and a musl loader;
#   * gles-gears, an ordinary GLES2 program cross-compiled for aarch64
#     against the standard headers.
#
# Then `gexc run --gl` gives the guest `/dev/gex-gl`, and the drawing happens
# on whatever OpenGL the host has.
#
# Nothing about the program is gex-aware. It includes <EGL/egl.h> and
# <GLES2/gl2.h>, links against the usual sonames, and would run unchanged on
# a phone.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
FRAMES="${1:-30}"
OUT="${2:-$ROOT/gears.ppm}"

WORK="${GEX_DEMO_WORK:-${TMPDIR:-/tmp}/gex-gl-demo}"
GEXC="$ROOT/target/release/gexc"
CC="${GEX_CC:-$HOME/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc}"
MUSL="$(dirname "$CC")/../aarch64-linux-musl/lib/libc.so"

for tool in "$GEXC" "$CC"; do
    [ -x "$tool" ] || { echo "missing $tool -- run: cargo build --release"; exit 1; }
done
[ -f "$MUSL" ] || { echo "missing the musl runtime at $MUSL"; exit 1; }
# The GL headers have no architecture in them, so the host's serve for a cross
# build. They are copied out rather than used in place so the host's libc
# headers are not dragged in alongside them.
[ -d /usr/include/GLES2 ] || {
    echo "missing GLES2 headers -- apt-get install libgles2-mesa-dev libegl1-mesa-dev"
    exit 1
}

echo "== building the guest's GL driver"
"$HERE/build.sh" "$WORK/driver" >/dev/null

echo "== assembling a root filesystem"
rm -rf "$WORK/root"
mkdir -p "$WORK/root/lib" "$WORK/root/usr/lib" "$WORK/include"
cp "$MUSL" "$WORK/root/lib/ld-musl-aarch64.so.1"
cp --preserve=links "$WORK/driver/libGLESv2.so.2" "$WORK/driver/libEGL.so.1" "$WORK/root/usr/lib/"
cp -r /usr/include/EGL /usr/include/GLES2 /usr/include/KHR "$WORK/include/" 2>/dev/null || true

echo "== building the gears for aarch64"
"$CC" -O2 -o "$WORK/root/gears" \
    "$ROOT/guest/gl/probes/gles-gears.c" \
    -I "$WORK/include" -L "$WORK/driver" -lEGL -lGLESv2 -lm

echo "== running under gex"
"$GEXC" run --gl --sysroot "$WORK/root" --env LD_LIBRARY_PATH=/usr/lib \
    "$WORK/root/gears" "$FRAMES" /gears.ppm

cp "$WORK/root/gears.ppm" "$OUT"
echo "== wrote $OUT"
# A PNG if anything here can make one, because far more things can open it.
if command -v convert >/dev/null 2>&1; then
    convert "$OUT" "${OUT%.ppm}.png" && echo "== wrote ${OUT%.ppm}.png"
elif command -v python3 >/dev/null 2>&1; then
    python3 - "$OUT" "${OUT%.ppm}.png" <<'PYEOF' 2>/dev/null && \
        echo "== wrote ${OUT%.ppm}.png" || true
import sys, struct, zlib
src, dst = sys.argv[1], sys.argv[2]
d = open(src, 'rb').read()
n = 0
for i, b in enumerate(d):
    if b == 0x0a:
        n += 1
        if n == 3:
            head, px = d[:i], d[i + 1:]
            break
w, h = (int(x) for x in head.split(b'\n')[1].split())
raw = b''.join(b'\x00' + px[y * w * 3:(y + 1) * w * 3] for y in range(h))
def chunk(tag, data):
    c = tag + data
    return struct.pack('>I', len(data)) + c + struct.pack('>I', zlib.crc32(c))
open(dst, 'wb').write(
    b'\x89PNG\r\n\x1a\n'
    + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0))
    + chunk(b'IDAT', zlib.compress(raw))
    + chunk(b'IEND', b''))
PYEOF
fi
