#!/bin/sh
# Browser benchmark gate for the stock STK -> gex-gl -> Mesa/gex_wgpu -> WebGPU
# path. (This line used to say MobileGL. It has not been that path for a long
# time and MobileGL cannot be selected at all any more; see the note below.)
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
# The backend is `mesa` and there is no longer another option. MobileGL is
# **disabled in the page**: `MobileGlDevice.create()` always throws, and
# `?gl=mobilegl` is ignored with a warning rather than honoured, so
# `GEX_STK_GL` cannot reach it and setting it to anything else measures
# nothing useful. The knob is kept only so a future backend has a place to go.
#
# This warning used to read "a run with the default backend measures
# presentation of a frame that has nothing in it", which was true when the
# default was MobileGL and has been inverted ever since the default became
# mesa -- it invited exactly the conclusion that this gate cannot be trusted,
# or worse, that MobileGL is the thing to try. It is not: it never renders
# SuperTuxKart, it is broken for this workload, and it is not a comparison
# either. The only working reference renderer for STK is virgl
# (tests/graphics-in-browser.sh and the virgl/wayland demos).
node "$ROOT/web/test/drive.mjs" supertuxkart \
    --query "gl=${GEX_STK_GL:-mesa}" --cmd "$STK_RACE" \
    --display-fps 55 --display-samples "${GEX_STK_SAMPLES:-15}" \
    --wall "${GEX_STK_WALL:-90}" --idle "${GEX_STK_IDLE:-90}" | tee "$LOG"

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
