#!/bin/sh
# Smoke test of gex-steam for Linux guests (guest/steam/build.sh): build it
# for i386 (Half-Life) and x86-64 (Baba Is You), and call it as a game
# does (src/steam-smoke.c), on the host.
#
#   tests/games/steam-smoke.sh [OUT_DIR]
#
# Needs STEAM_API_JSON (default ~/src/Steamworks.NET/CodeGen/steam/steam_api.json)
# and clang with -m32 support.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
# GEX_ROOT: the tree whose guest/steam to build (default this one).
root=${GEX_ROOT:-$(cd "$here/../.." && pwd)}
out=${1:-${TMPDIR:-/tmp}/gex-steam-smoke}
fail=0
for a in 32 64; do
    "$root/guest/steam/build.sh" "$out/$a" clang++ -m$a > /dev/null
    clang -m$a -O1 -o "$out/$a/smoke" "$here/src/steam-smoke.c" -ldl
    if "$out/$a/smoke" "$out/$a/libsteam_api.so"; then echo "ok   gex-steam $a-bit"; else echo "FAIL gex-steam $a-bit"; fail=1; fi
done
exit $fail
