#!/bin/sh
# Build gex-steam's libsteam_api.so for a guest.
#
#   guest/steam/build.sh OUT_DIR [CXX...]
#
# STEAM_API_JSON names the Steamworks SDK's steam_api.json, which the
# interface classes are generated from (the SDK is not part of this tree).
# The default compiler builds for i386 (GoldSrc's hl_linux); pass another,
# e.g. `g++` for x86-64.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:?usage: build.sh OUT_DIR [CXX...]}"
shift
[ $# -gt 0 ] || set -- g++ -m32
JSON="${STEAM_API_JSON:-$HOME/src/Steamworks.NET/CodeGen/steam/steam_api.json}"
[ -f "$JSON" ] || { echo "no steam_api.json at $JSON (set STEAM_API_JSON)" >&2; exit 2; }
mkdir -p "$OUT"
python3 "$HERE/gen.py" "$JSON" "$OUT/steam_interfaces.inc"
# No C++ runtime: the classes have no RTTI or exceptions, so the library
# needs only libc.
"$@" -shared -fPIC -O2 -Wall -Wno-unused-parameter -fno-exceptions -fno-rtti -nostdlib++ \
    -fvisibility=hidden -I"$OUT" -Wl,-soname,libsteam_api.so \
    -o "$OUT/libsteam_api.so" "$HERE/gexsteam.cpp"
echo "$OUT/libsteam_api.so"
