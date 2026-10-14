#!/bin/sh
# Copies the gex WebGPU wire into a Wine tree's dlls/webgpu (whose own files,
# Makefile.in, webgpu_main.c, unix_webgpu.c and unixlib.h, live in the Wine
# fork) and its webgpu.h into include/, and writes webgpu.spec.
#   sync-webgpu.sh WINE_SOURCE_TREE
set -eu
GEX=$(cd "$(dirname "$0")/../.." && pwd)
WINE=$1
DLL=$WINE/dlls/webgpu
python3 "$GEX/tools/gen_webgpu_wire.py" >/dev/null
mkdir -p "$DLL/gen"
cp "$GEX/guest/gpu/wire.h" "$GEX/guest/gpu/wire-client.c" "$GEX/guest/gpu/wire-server.c" \
   "$GEX/guest/gpu/wire-server.h" "$DLL/"
cp "$GEX/guest/gpu/gen/wire-gen.h" "$GEX/guest/gpu/gen/wire-client-gen.inc" \
   "$GEX/guest/gpu/gen/wire-server-gen.inc" "$DLL/gen/"
cp "$GEX/third_party/dawn-api/webgpu.h" "$WINE/include/webgpu.h"
# Every entry point the client defines, by name; their arguments are C's
# business (the relay does not see them).
sed -n 's/^#define \(wgpu[A-Za-z0-9]*\) gex_.*/@ cdecl -norelay \1()/p' \
    "$GEX/guest/gpu/gen/wire-rename-gen.h" > "$DLL/webgpu.spec"
echo "$(wc -l < "$DLL/webgpu.spec") exports"
