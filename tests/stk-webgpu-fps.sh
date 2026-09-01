#!/bin/sh
# Browser benchmark gate for the stock STK -> gex-gl -> MobileGL -> WebGPU path.
# It runs an ordinary unbounded race and stops from the browser harness. STK's
# profile mode counts simulation steps rather than eglSwapBuffers and is not a
# valid measurement of real canvas presents.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
LOG=$(mktemp "${TMPDIR:-/tmp}/gex-stk-webgpu-fps.XXXXXX")
trap 'rm -f "$LOG"' EXIT INT TERM

STK_RACE='/usr/games/supertuxkart --race-now --track=snowtuxpeak --kart=tux --numkarts=1 --laps=99 --screensize=640x480 --windowed --disable-hd-textures --disable-glow --disable-bloom --disable-light-shaft --disable-dof --disable-particles --disable-animated-characters --disable-motion-blur --disable-mlaa --disable-ssao --disable-ibl --shadows=0 --anisotropic=0'

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
# Which GL backend decodes the guest's command ring.
#
# Deploying a rebuilt driver: web/test/drive.mjs serves web/dist, NOT web/public.
# Copying gex-wgpu-replay.{js,wasm} into web/public/gex alone leaves the browser
# running the previous build with no sign that it did -- a whole run's worth of
# diagnostics then describes code that is no longer in the tree. Copy into both
# web/public/gex and web/dist/gex.
#
# WARNING: MobileGL does NOT render SuperTuxKart -- it is broken for this
# workload, so a run with the default backend measures presentation of a frame
# that has nothing in it. `GEX_STK_GL=mesa` is the path under development and
# the one worth gating. The only working reference renderer for STK is virgl
# (tests/graphics-in-browser.sh and the virgl/wayland demos).
node "$ROOT/web/test/drive.mjs" supertuxkart \
    --query "gl=${GEX_STK_GL:-mesa}" --cmd "$STK_RACE" \
    --display-fps 55 --display-samples 15 --wall 90 --idle 90 | tee "$LOG"

TIMING=$(grep 'Present timing (' "$LOG" | tail -n 1 || true)
[ -n "$TIMING" ] || { echo 'missing presentation timing result' >&2; exit 1; }

DURATION=$(printf '%s\n' "$TIMING" | sed -E 's/.*Present timing \(([0-9]+)ms\).*/\1/')
FRESH=$(printf '%s\n' "$TIMING" | sed -E 's/.*fresh=([0-9]+)\/([0-9]+).*/\1/')
TICKS=$(printf '%s\n' "$TIMING" | sed -E 's/.*fresh=([0-9]+)\/([0-9]+).*/\2/')
MISSES=$(printf '%s\n' "$TIMING" | sed -E 's/.*misses=([0-9]+).*/\1/')

awk -v duration="$DURATION" -v fresh="$FRESH" -v ticks="$TICKS" -v misses="$MISSES" \
    -v backend="${GEX_STK_GL:-mesa}" 'BEGIN {
    fps = fresh * 1000.0 / duration
    coverage = fresh / ticks
    printf "STK real-present gate (%s): %.2f FPS, %.2f%% fresh, %d rAF misses\n", backend, fps, coverage * 100, misses
    if (duration < 9900 || fps < 59.8 || coverage < 0.995 || misses > 1) exit 1
}'
