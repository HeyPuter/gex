#!/bin/bash
# Peggle Deluxe on gex as a workload (docs/games.md): from a profile-less
# start through the name entry into Stage One, shooting balls, with the
# scripted input of tests/games/peggle-input.txt (GEX_NT_INPUT, timed by GL
# presents, so a run is the same each time and in both places).
#
#   tests/games/peggle.sh [--browser] [--measure S] [--shot DIR] [-- GEXC-FLAGS]
#
# Native (default): gexc runs the shipped Peggle.exe in its JIT tier (--no-cgen;
# GEXC-FLAGS replace that). --browser: tests/games/browser-peggle.mjs, the
# same game and script in web/nt.html (GEX_WEBGPU_SOFTWARE=0 for the real
# GPU).
#
# Prints, as `ok` lines: seconds to the first frame, seconds to gameplay
# (the frame the script's `# gameplay N` names), and frames/s over --measure
# seconds of gameplay (default 60). --shot DIR keeps gameplay.png there.
#
# Env: GEXC (default target/x86/release/gexc), PEGGLE_DIR (a copy of the
# game as shipped, run unmodified: Peggle.exe is PopCap's wrapper, which
# starts the game itself; default ../../work/games/peggle from the repo root),
# GEX_WINE_PREFIX (a staged prefix, tools/nt-stage-wine.sh), CORES (default
# 5,11).
#
# Native runs write the profile into the prefix; the run uses a linked copy
# of it (in $TMPDIR) whose ProgramData is emptied each time.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
browser=0 measure=60 shot= flags=(--no-cgen)
while [ $# -gt 0 ]; do
    case $1 in
        --browser) browser=1 ;;
        --measure) measure=$2; shift ;;
        --shot) shot=$2; shift ;;
        --) shift; flags=("$@"); break ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done
game=${PEGGLE_DIR:-$root/../../work/games/peggle}
prefix=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
[ -f "$game/Peggle.exe" ] || { echo "no $game/Peggle.exe" >&2; exit 2; }
[ -f "$prefix/.wine-version" ] || { echo "no staged Wine prefix at $prefix (tools/nt-stage-wine.sh)" >&2; exit 2; }

if [ $browser = 1 ]; then
    exec node "$here/browser-peggle.mjs" --peggle "$game" --measure "$measure" ${shot:+--shot "$shot"}
fi

gexc=${GEXC:-$root/target/x86/release/gexc}
[ -x "$gexc" ] || { echo "no $gexc (cargo build --release -p gexc --features gexc/x86 --target-dir target/x86)" >&2; exit 2; }
state=${TMPDIR:-/tmp}/gex-games-$(id -u)
run=$state/peggle-prefix
mkdir -p "$state"
if [ ! -f "$run/.wine-version" ] || [ "$prefix/.wine-version" -nt "$run/.wine-version" ]; then
    rm -rf "$run"
    # Hard links, or symlinks across filesystems: what Peggle writes is new
    # files under ProgramData, which is the run's own.
    cp -al "$prefix" "$run" 2>/dev/null || { rm -rf "$run"; cp -as "$(cd "$prefix" && pwd)" "$run"; }
fi
rm -rf "$run/drive_c/ProgramData"
mkdir -p "$run/drive_c/ProgramData"
out=$state/peggle-frames
rm -rf "$out"; mkdir -p "$out"
log=$state/peggle.log

input=$(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$here/peggle-input.txt" | tr -d ' ' | paste -sd';')
gameplay=$(sed -n 's/^# gameplay \([0-9]*\)$/\1/p' "$here/peggle-input.txt")
# Frames are written every 10th (the last one is the screenshot): a full
# 1920x1080 frame is 6 MB.
cd "$game"
t0=$(date +%s%N)
GEX_NT_STRICT=1 GEX_NT_STATS=1 GEX_PPM_EVERY=10 GEX_NT_INPUT="$input" \
    taskset -c "${CORES:-5,11}" "$gexc" run "${flags[@]}" --wine-prefix "$run" --gl \
    --wayland-out "$out/f" Peggle.exe > "$log" 2>&1 &
pid=$!

# GEX_NT_STATS prints `t=MS presents=N` each second.
first= play= playn= result=
while kill -0 $pid 2>/dev/null; do
    line=$(grep -a 'gex-nt: stats' "$log" | tail -1)
    if [[ $line =~ t=([0-9]+)\ presents=([0-9]+) ]]; then
        t=${BASH_REMATCH[1]} n=${BASH_REMATCH[2]}
        now=$(( ($(date +%s%N) - t0) / 1000000 ))
        if [ -z "$first" ] && [ "$n" -gt 0 ]; then
            first=$now
            echo "ok   first frame after $((first / 1000)) s"
        fi
        if [ -z "$play" ] && [ "$n" -ge "$gameplay" ]; then
            play=$t playn=$n
            echo "ok   gameplay (frame $gameplay) after $((now / 1000)) s"
        fi
        if [ -n "$play" ] && [ $((t - play)) -ge $((measure * 1000)) ]; then
            result=$(awk -v n=$((n - playn)) -v ms=$((t - play)) 'BEGIN { printf "%.1f frames/s over %d s", n * 1000 / ms, ms / 1000 }')
            break
        fi
    fi
    # Only the newest frame is kept (the screenshot).
    ls "$out" | grep '\.ppm$' | head -n -1 | sed "s|^|$out/|" | xargs -r rm -f
    sleep 1
done
# TERM: a background job of a script ignores SIGINT.
kill -TERM $pid 2>/dev/null
wait $pid 2>/dev/null
if [ -z "$result" ]; then
    echo "FAIL peggle stopped before $measure s of gameplay; $log ends:"
    tail -5 "$log"
    exit 1
fi
echo "ok   gameplay: $result"
if [ -n "$shot" ]; then
    mkdir -p "$shot"
    last=$(ls "$out" | grep '\.ppm$' | tail -1)
    python3 "$here/ppm2png.py" "$out/$last" "$shot/gameplay.png" && echo "     $shot/gameplay.png"
fi
rm -rf "$out"
