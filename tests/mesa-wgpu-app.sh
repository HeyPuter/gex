#!/bin/sh
# Run a real guest application against Mesa + gex_wgpu on WebGPU.
#
#   tests/mesa-wgpu-app.sh [demo] [seconds]
#
# The point of this over tests/mesa-wgpu-live.sh is the workload: that one runs
# a small demo written for this project, and this one runs a stock aarch64
# application -- SuperTuxKart by default -- through the identical wire
# protocol. An application's state churn is nothing like a demo's, and it finds
# driver defects a narrow test never reaches.
#
# Staging is the whole reason this exists as a script. web/test/drive.mjs
# serves web/public and web/dist, and neither the Mesa build nor drive.mjs
# copies the driver there, so driving directly measures whatever wasm was
# staged last -- silently, and with every appearance of having worked.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
DEMO=${1:-supertuxkart}
SECONDS_WANTED=${2:-120}
BUILD=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}/build/src/gallium/targets/gex-wasm
WORK=${TMPDIR:-/tmp}/gex-mesa-wgpu-app
mkdir -p "$WORK"

[ -f "$BUILD/gex-wgpu-replay.wasm" ] || {
    echo "skip: no gex-wgpu-replay; run web/build-mesa-wasm.sh" >&2
    exit 0
}

for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    [ -d "$tree" ] || continue
    mkdir -p "$tree/gex"
    cp "$BUILD/gex-wgpu-replay.js" "$BUILD/gex-wgpu-replay.wasm" "$tree/gex/"
done

# The guest's own GL driver too, and the glibc build of it, which is the one
# this demo loads.
#
# Staging only the host module leaves the application running whatever
# marshaller web/build-assets.sh last produced. That is not a theoretical
# staleness: SuperTuxKart spent days rendering its scene almost entirely black
# because the staged guest driver predated the fix that lets the guest report
# the host's extensions, so STK could not see EXT_texture_compression_s3tc and
# every DXT texture in the track decoded to nothing. The host module was
# current the whole time and said so.
#
# A demo marked `directGlAbi: glibc` reads /fs/glibc/*; the musl pair at /fs is
# for the small demos. Both are refreshed because both are served from here.
GUEST_GL=$WORK/guest-gl
GUEST_GL_GLIBC=$WORK/guest-gl-glibc
"$ROOT/guest/gl/build.sh" "$GUEST_GL" >/dev/null
if command -v strat >/dev/null 2>&1 &&
   strat debian test -x /usr/bin/aarch64-linux-gnu-gcc; then
    strat debian env GEX_CC=/usr/bin/aarch64-linux-gnu-gcc \
        "$ROOT/guest/gl/build.sh" "$GUEST_GL_GLIBC" >/dev/null
elif command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
    GEX_CC="$(command -v aarch64-linux-gnu-gcc)" \
        "$ROOT/guest/gl/build.sh" "$GUEST_GL_GLIBC" >/dev/null
else
    echo "warning: no aarch64-linux-gnu-gcc; the glibc guest GL driver this" \
         "demo loads is whatever was staged last" >&2
    GUEST_GL_GLIBC=
fi
for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    [ -d "$tree" ] || continue
    mkdir -p "$tree/fs" "$tree/fs/glibc"
    cp "$GUEST_GL/libGLESv2.so.2" "$GUEST_GL/libEGL.so.1" "$tree/fs/"
    [ -z "$GUEST_GL_GLIBC" ] || cp "$GUEST_GL_GLIBC/libGLESv2.so.2" \
        "$GUEST_GL_GLIBC/libEGL.so.1" "$GUEST_GL_GLIBC/libGL.so.1" \
        "$tree/fs/glibc/"
done

# Overridable with GEX_STK_CMD. The default strips almost every renderer
# feature so the benchmark measures the driver rather than SuperTuxKart's
# post-processing -- but that also removes its ambient lighting, so a dark
# scene under these flags is not by itself a rendering fault. Run with a
# fuller flag set before concluding anything about lighting.
STK_RACE=${GEX_STK_CMD:-}
[ -n "$STK_RACE" ] || STK_RACE='/usr/games/supertuxkart --race-now --track=snowtuxpeak --kart=tux --numkarts=1 --laps=99 --screensize=640x480 --windowed --disable-hd-textures --disable-glow --disable-bloom --disable-light-shaft --disable-dof --disable-particles --disable-animated-characters --disable-motion-blur --disable-mlaa --disable-ssao --disable-ibl --shadows=0 --anisotropic=0'

# `es2gears` is the other application here: mesa-demos' gears, unmodified, as
# an EGL-on-X11 client under Xwayland. It covers a path SuperTuxKart does not
# -- SDL hides most of EGL from STK, and this one asks EGL directly, which is
# how the missing EGL_NATIVE_VISUAL_ID was found. The *native Wayland* build
# of the same program cannot run here: this project's EGL has no Wayland
# platform, so it renders one frame and waits for a frame callback forever.
#
# The frame bounds differ per application because the pictures do. Gears is
# three shapes on black -- most of the frame legitimately is dark -- while a
# dark SuperTuxKart frame means something failed.
PAGE=$DEMO
case $DEMO in
supertuxkart)
    FRAME_BOUNDS="--max-dark 0.20 --min-stdev 0.08 --min-distinct 10"
    ;;
es2gears)
    PAGE=wayland
    STK_RACE=${GEX_STK_CMD:-/usr/bin/es2gears_x11.aarch64-linux-gnu}
    FRAME_BOUNDS="--max-dark 0.85 --min-stdev 0.08 --min-distinct 8"
    ;;
glmark2)
    # The one application here that comes in through GLX, and the heaviest.
    # Its scenes vary from a dark model on black to a full-screen texture, so
    # the bounds are loose: what this gate is for is that GLX still works at
    # all and that the canvas is not blank.
    PAGE=wayland
    STK_RACE=${GEX_STK_CMD:-'glmark2 -b build:duration=5'}
    FRAME_BOUNDS="--max-dark 0.97 --min-stdev 0.03 --min-distinct 3"
    ;;
es2tri)
    # One Gouraud-shaded triangle on a flat grey clear, fed by a client-side
    # vertex array. `distinct` is what carries the check here: the clear alone
    # reaches 4 cells of the colour cube and the triangle takes it to 19, so a
    # regression in client arrays shows as a number, not as a look.
    PAGE=wayland
    STK_RACE=${GEX_STK_CMD:-/usr/bin/es2tri.aarch64-linux-gnu}
    FRAME_BOUNDS="--max-dark 0.30 --min-stdev 0.03 --min-distinct 12"
    ;;
*)
    FRAME_BOUNDS="--max-dark 0.85 --min-stdev 0.05 --min-distinct 4"
    ;;
esac

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
    node "$ROOT/web/test/drive.mjs" "$PAGE" \
    --query 'gl=mesa' --cmd "$STK_RACE" \
    --display-fps 1 --display-samples "$SECONDS_WANTED" \
    --shot "$WORK/$DEMO.png" \
    --wall $((SECONDS_WANTED * 8)) --idle $((SECONDS_WANTED * 5)) \
    >"$WORK/drive.log" 2>&1 || true

grep -oE "gex-wgpu: (negotiated|application asked|first refusal)[^\"]{0,80}" \
    "$WORK/drive.log" | sort -u | sed 's/^/     /' || true

# A refusal on a real application is a defect by construction, and needs no
# reference renderer to say so: the driver was asked for something legal and
# said no, which means pixels are missing or the context is dead. This is the
# cheapest strong signal available and it was already being printed and
# ignored.
#
# Deliberately not `render target refused <format>`: that line is
# is_format_supported answering a question, which st asks about every format
# there is, and a "no" there is a fact rather than a failure.
REFUSALS=$(grep -aoE "gex-wgpu: (first refusal at [a-zA-Z_0-9:]+|draw declined \([^)]*\)|[a-z ]*shader (rejected|declined)[^\"]{0,60})" \
    "$WORK/drive.log" | sort -u || true)
DECLINED=$(grep -aoE "declined [1-9][0-9]*" "$WORK/drive.log" | sort -u || true)
if [ -n "$REFUSALS" ] || [ -n "$DECLINED" ]; then
    echo "FAIL mesa-wgpu-app: the driver refused something this application asked for"
    printf '%s\n' "$REFUSALS" "$DECLINED" | grep -v '^$' | sed 's/^/     /'
    exit 1
fi

# The frame counter is the verdict, not the harness's exit code. Its FPS goal
# counts canvas animation ticks, which keep ticking at 60Hz over a canvas that
# never receives a new frame -- so a run that draws nothing at all still
# reports PASS and a plausible frame rate.
TIMING=$(grep -oE 'fresh=[0-9]+/[0-9]+' "$WORK/drive.log" | tail -1 || true)
[ -n "$TIMING" ] || { echo "FAIL mesa-wgpu-app: no presentation timing"; exit 1; }
FRESH=${TIMING#fresh=}
FRESH=${FRESH%%/*}
echo "     presented $TIMING"

# And what the frame actually looks like. The counter above cannot tell a
# rendered scene from a black one: SuperTuxKart presented 600 of 600 frames
# for days while almost every surface in it decoded to nothing, because the
# staged guest GL driver was old enough that the application could not see
# EXT_texture_compression_s3tc.
#
# The bounds describe "a rendered scene", not one particular frame, and are
# chosen per application above. A good STK frame measures dark=0.006
# stdev=0.183 distinct=29 and a scene whose textures fail is mostly black,
# flat and reduced to a handful of colours; es2gears measures dark=0.701
# stdev=0.172 distinct=13 because most of its frame is background.
# drive.mjs clips its screenshot to the canvas, so this reads the frame and
# nothing else.
if command -v python3 >/dev/null 2>&1 && [ -f "$WORK/$DEMO.png" ]; then
    if python3 "$ROOT/tools/frame-stats.py" $FRAME_BOUNDS \
            "$WORK/$DEMO.png" | sed 's/^/     /'; then
        :
    else
        echo "FAIL mesa-wgpu-app: the frame does not look like a rendered scene"
        echo "     look at $WORK/$DEMO.png"
        exit 1
    fi
fi
if [ "$FRESH" -eq 0 ]; then
    echo "FAIL mesa-wgpu-app: the application never presented a frame"
    exit 1
fi

# What this number is, and is not. `fresh` counts the backend's swap counter
# advancing: the application ran a frame to completion and the drawable was
# copied to the canvas. It does not say the frame was *correct* -- a copy of a
# wrong texture advances it just as well -- so a visual check against the
# harness screenshot is still the thing that confirms rendering.
#
# It used to mean even less: display_backend() in the Mesa build counted
# frames and copied no pixels at all, so a run reported every frame fresh over
# a canvas that stayed black, and did.
echo "ok   mesa-wgpu-app ($DEMO presented $FRESH frames)"
echo "     note: frames reached the canvas and the frame reads as a rendered" \
     "scene; anything finer than that still needs the screenshot at" \
     "$WORK/$DEMO.png"
