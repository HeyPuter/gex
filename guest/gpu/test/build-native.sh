#!/bin/sh
# Builds the wire's client, server and in-process glue against a native
# webgpu.h (Dawn's prebuilt release): libgexgpu-native.a, for linking into a
# webgpu.h program compiled with -include gen/wire-rename-gen.h.
#   build-native.sh OUTDIR
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
GPU=$(dirname "$HERE")
ROOT=$(dirname "$(dirname "$GPU")")
OUT=${1:-$ROOT/target/gexgpu-native}
CC=${CC:-cc}
CFLAGS="-O2 -g -Wall -Wno-unused-function -I$GPU -I$ROOT/third_party/dawn-api"
mkdir -p "$OUT"
python3 "$ROOT/tools/gen_webgpu_wire.py" >/dev/null
$CC $CFLAGS '-DGEXGPU_CLIENT_NAME(n)=gex_##n' -c "$GPU/wire-client.c" -o "$OUT/wire-client.o"
$CC $CFLAGS -c "$GPU/wire-server.c" -o "$OUT/wire-server.o"
$CC $CFLAGS -c "$HERE/native-glue.c" -o "$OUT/native-glue.o"
rm -f "$OUT/libgexgpu-native.a"
# Apple's linker reads only Apple's archives; GNU ar may come first on PATH.
if [ "$(uname)" = Darwin ]; then
    /usr/bin/libtool -static -o "$OUT/libgexgpu-native.a" "$OUT/wire-client.o" "$OUT/wire-server.o" "$OUT/native-glue.o"
else
    ar rcs "$OUT/libgexgpu-native.a" "$OUT/wire-client.o" "$OUT/wire-server.o" "$OUT/native-glue.o"
fi
echo "$OUT/libgexgpu-native.a"
