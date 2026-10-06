#!/bin/sh
# Build gex-steam for Windows games (docs/games.md, "gex-steam"):
# steam_api.dll (i386) and steam_api64.dll (x86-64), with every interface
# version of the Steamworks SDK.
#
#   guest/steam/build-win.sh OUT_DIR
#
# STEAM_SDK_HEADERS: interface headers of every version (the current SDK's
# isteam*.h plus isteamuser018.h and the like; the Goldberg emulator's
# sdk/steam has them). STEAM_API_JSON: the current SDK's steam_api.json.
# Neither is part of this tree. Needs clang (MSVC targets), lld, and
# MinGW-w64's kernel32 import libraries.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:?usage: build-win.sh OUT_DIR}"
SDK="${STEAM_SDK_HEADERS:-$HOME/src/gbe_fork/sdk/steam}"
JSON="${STEAM_API_JSON:-$HOME/src/Steamworks.NET/CodeGen/steam/steam_api.json}"
CXX="${CXX:-clang++}"
# kernel32's import library: MinGW's (lld links against it as is).
KERNEL32_DIR="${KERNEL32_DIR:-/usr}"
LLD="${LLD:-$(command -v lld-link || echo "$HOME/emsdk/upstream/bin/lld -flavor link")}"
[ -d "$SDK" ] || { echo "no interface headers at $SDK (set STEAM_SDK_HEADERS)" >&2; exit 2; }
[ -f "$JSON" ] || { echo "no steam_api.json at $JSON (set STEAM_API_JSON)" >&2; exit 2; }
mkdir -p "$OUT/i386" "$OUT/x64"
python3 "$HERE/gen_versions.py" "$SDK" "$JSON" msvc "$OUT/steam_versions.inc"
for a in i386 x64; do
    case $a in
        i386) target=i686-pc-windows-msvc machine=x86 name=steam_api.dll extra=/safeseh:no mingw=i686-w64-mingw32 ;;
        x64) target=x86_64-pc-windows-msvc machine=x64 name=steam_api64.dll extra= mingw=x86_64-w64-mingw32 ;;
    esac
    "$CXX" --target=$target -std=c++20 -O2 -fno-exceptions -fno-rtti -ffreestanding -fno-stack-protector \
        -mno-stack-arg-probe -Wall -Wno-unused-function -I"$OUT" -c "$HERE/gexsteam_win.cpp" -o "$OUT/$a/gexsteam.obj"
    $LLD /dll /noentry /nodefaultlib $extra /machine:$machine /out:"$OUT/$a/$name" \
        "$OUT/$a/gexsteam.obj" "$KERNEL32_DIR/$mingw/lib/libkernel32.a"
    echo "$OUT/$a/$name"
done
