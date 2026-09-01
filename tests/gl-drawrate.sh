#!/bin/sh
# How many draw calls a second does the whole stack manage, and how does that
# compare with a reference driver on the same machine?
#
#   tests/gl-drawrate.sh [draws-per-frame]
#
# Twenty seconds against the five minutes a game takes to reach a comparable
# scene, and it answers the only question that matters when a frame is made of
# thousands of small objects. softpipe is the reference: it rasterises on the
# CPU and is still the fastest thing here at issuing draws, so any large gap
# is the driver's per-draw cost rather than st/mesa's.
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${TMPDIR:-/tmp}/gex-drawrate
DRAWS=${1:-2000}
mkdir -p "$OUT"
for src in gl-drawrate gl-drawrate-state; do
    cc -O2 -o "$OUT/$src" "$ROOT/tests/$src.c" -lGL -lX11
done
for driver in gex_wgpu softpipe; do
    for mode in 1 7; do
        printf '%-10s state=%s  ' "$driver" "$mode"
        GALLIUM_DRIVER=$driver timeout 20 stdbuf -oL \
            "$ROOT/web/run-native-gl.sh" "$OUT/gl-drawrate-state" "$DRAWS" "$mode" \
            2>/dev/null | grep -oE '^  [0-9]+ draws/s' | tail -1
    done
done
