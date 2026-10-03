#!/bin/bash
# Half-Life on gex as a workload (docs/games.md): GoldSrc (i386 Linux, SDL2,
# OpenGL) loads a map and runs it. c1a0 is the tram ride, which plays the
# same way with no input, so the run is repeatable.
#
#   tests/games/halflife.sh [--browser] [--map M] [--measure S] [--shot DIR]
#                           [-- GEXC-FLAGS]
#
# Native (default): gexc runs hl_linux in its JIT tier (--no-cgen; GEXC-FLAGS
# replace that) with its window on gexc's Wayland compositor and CEF's X
# display from an Xvfb the script starts (or HL_DISPLAY, over TCP).
# --browser: tests/games/browser-halflife.mjs (the page's own X server;
# -nomsaa, see docs/games.md; GEX_WEBGPU_SOFTWARE=0 for the real GPU).
#
# Prints, as `ok` lines: seconds until the map is loaded and frames/s over
# --measure seconds after it (default 60). --shot DIR keeps map.png there.
#
# Env:
#   GEXC            default target/x86/release/gexc
#   HL_DIR          a copy of the Steam install (the game writes into it);
#                   default ../../work/games/halflife from the repo root
#   GEX_GL_I386     guest/gl built for i386 (GEX_CC="gcc -m32" guest/gl/build.sh)
#   GEX_STEAM_I386  guest/steam/build.sh for i386
#   GEX_HL_FALLBACK the Steam runtime's GTK2 libraries CEF loads, linked into
#                   one directory
#   (the three default to ../../work/games/deps/{gl-i386,steam-i386,hl-fallback})
#   HL_DISPLAY      an X display the guest reaches over TCP (default: an
#                   Xvfb on :HL_XVFB, default 93, for the run)
#   CORES           default 5,11
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$root/../../work/games
browser=0 map=c1a0 measure=60 shot= flags=(--no-cgen)
while [ $# -gt 0 ]; do
    case $1 in
        --browser) browser=1 ;;
        --map) map=$2; shift ;;
        --measure) measure=$2; shift ;;
        --shot) shot=$2; shift ;;
        --) shift; flags=("$@"); break ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done
export HL_DIR=${HL_DIR:-$work/halflife}
export GEX_GL_I386=${GEX_GL_I386:-$work/deps/gl-i386}
export GEX_STEAM_I386=${GEX_STEAM_I386:-$work/deps/steam-i386}
export GEX_HL_FALLBACK=${GEX_HL_FALLBACK:-$work/deps/hl-fallback}
for d in "$HL_DIR/hl_linux" "$GEX_GL_I386/libGL.so.1" "$GEX_STEAM_I386/libsteam_api.so"; do
    [ -e "$d" ] || { echo "no $d (see the header)" >&2; exit 2; }
done

if [ $browser = 1 ]; then
    # Chromium's socket path must be short.
    export TMPDIR=${TMPDIR:-/tmp/gexg}
    mkdir -p "$TMPDIR"
    exec node "$here/browser-halflife.mjs" --map "$map" --measure "$measure" --hl-args -nomsaa ${shot:+--shot "$shot"}
fi

gexc=${GEXC:-$root/target/x86/release/gexc}
[ -x "$gexc" ] || { echo "no $gexc (cargo build --release -p gexc --features gexc/x86 --target-dir target/x86)" >&2; exit 2; }
state=${TMPDIR:-/tmp}/gex-games-$(id -u)
out=$state/halflife-frames
log=$state/halflife.log
mkdir -p "$state/hlhome"
rm -rf "$out"; mkdir -p "$out"

# CEF (chromehtml.so, loaded every time) needs an X display, which the
# guest reaches over TCP: HL_DISPLAY, or an Xvfb of our own.
xvfb=
if [ -z "${HL_DISPLAY:-}" ]; then
    n=${HL_XVFB:-93}
    Xvfb :$n -listen tcp -nolisten unix -screen 0 800x600x24 > "$state/xvfb.log" 2>&1 &
    xvfb=$!
    HL_DISPLAY=127.0.0.1:$n
    sleep 2
fi

cd "$HL_DIR"
t0=$(date +%s%N)
# Every 10th frame is written, named by its real index: the newest name is
# the frame count.
GEX_PPM_EVERY=10 taskset -c "${CORES:-5,11}" "$gexc" run "${flags[@]}" --gl \
    --wayland /tmp/gex-hl-wl-$(id -u) --wayland-size 800x600 --wayland-out "$out/f" \
    --env LD_LIBRARY_PATH="$GEX_STEAM_I386:$GEX_GL_I386:$HL_DIR:$GEX_HL_FALLBACK" \
    --env SDL_VIDEODRIVER=wayland --env SDL_AUDIODRIVER=dummy --env HOME="$state/hlhome" --env DISPLAY="$HL_DISPLAY" \
    ./hl_linux -novid -nojoy +map "$map" > "$log" 2>&1 &
pid=$!

frames() { ls "$out" | sed -n 's/^f-0*\([0-9][0-9]*\)\.ppm$/\1/p' | sort -n | tail -1; }
up= upn= result=
while kill -0 $pid 2>/dev/null; do
    now=$(( ($(date +%s%N) - t0) / 1000000 ))
    if [ -z "$up" ] && grep -aqE 'Unknown command: VModEnable|loaded map|Loading map' "$log"; then
        up=$now upn=$(frames)
        echo "ok   $map is loaded after $((now / 1000)) s"
    fi
    if [ -n "$up" ] && [ $((now - up)) -ge $((measure * 1000)) ]; then
        n=$(frames)
        result=$(awk -v n=$((${n:-0} - ${upn:-0})) -v ms=$((now - up)) 'BEGIN { printf "%.1f frames/s over %d s", n * 1000 / ms, ms / 1000 }')
        break
    fi
    # Only the newest frame is kept (the screenshot).
    last=$(frames)
    [ -n "$last" ] && ls "$out" | grep '\.ppm$' | grep -v "^f-0*$last\.ppm$" | sed "s|^|$out/|" | xargs -r rm -f
    sleep 1
done
# TERM: a background job of a script ignores SIGINT.
kill -TERM $pid 2>/dev/null
wait $pid 2>/dev/null
[ -n "$xvfb" ] && kill $xvfb 2>/dev/null
if [ -z "$result" ]; then
    echo "FAIL half-life stopped before $measure s on $map; $log ends:"
    tail -5 "$log"
    exit 1
fi
echo "ok   $map: $result"
if [ -n "$shot" ]; then
    mkdir -p "$shot"
    last=$(ls "$out" | grep '\.ppm$' | head -1)
    python3 "$here/ppm2png.py" "$out/$last" "$shot/map.png" && echo "     $shot/map.png"
fi
rm -rf "$out"
