#!/bin/sh
# The graphics stack in the browser runtime, stage by stage.
#
#   tests/graphics-in-browser.sh [x|gears] [seconds]
#
# The browser runtime under Node: the same `gex.mjs` a page loads, the same
# shim, the same runtime translator. What it cannot do is draw -- there is no
# WebGL here -- so `gears` gets as far as Mesa asking for a socket that nobody
# serves. `web/test/virgl-gears.mjs` is the one with a GPU behind it.
#
# What this is for is the part that has nothing to do with graphics: an X
# server, its `xkbcomp`, a shell and a client, all as separate processes in one
# address space. Every one of those was a defect at some point, and each shows
# up here as a stage that is never reached.
#
# It reports as it goes. A run takes ten minutes or more -- Mesa, libX11 and an
# X server are translated a basic block at a time -- and a harness that says
# nothing until the end cannot tell "slow" from "wedged", which cost several
# afternoons before this existed.
set -eu

WHAT="${1:-x}"
SECONDS_ARG="${2:-20}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
R="${GEX_GRAPHICS_ROOT:-/tmp/gex-assets/graphics/root}"
SEED="$ROOT/web/public/gex/seed.wasm"
LIMIT="${GEX_LIMIT:-2400}"

[ -x "$R/usr/bin/Xvfb" ] || { echo "skip: no graphics rootfs at $R (web/build-graphics-rootfs.sh)"; exit 0; }
[ -f "$SEED" ] || { echo "skip: no seed module (web/build-assets.sh)"; exit 0; }
command -v node >/dev/null 2>&1 || { echo "skip: no node"; exit 0; }

case "$WHAT" in
    x)
        cat > "$R/stages.sh" <<'EOF'
export DISPLAY=:0
export LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu
echo "stage: starting the X server"
Xvfb :0 -screen 0 300x300x24 -nolisten tcp &
i=0
while [ ! -e /tmp/.X11-unix/X0 ]; do
  i=$((i + 1))
  [ "$i" -gt 300 ] && { echo "stage: NO X SOCKET"; exit 1; }
  sleep 1
done
echo "stage: X socket up"
sleep 3
echo "stage: X server still alive"
EOF
        WANT="X server still alive"
        ;;
    gears)
        cat > "$R/stages.sh" <<EOF
GEARS_SECONDS=$SECONDS_ARG
export GEARS_SECONDS
echo "stage: running the gears script"
. /gears.sh
EOF
        WANT="frames in"
        ;;
    *) echo "usage: graphics-in-browser.sh [x|gears] [seconds]"; exit 2 ;;
esac

out="${TMPDIR:-/tmp}/gex-graphics-stages.$$"
start=$(date +%s)
# Line-buffered through awk, which stamps each stage with how long it took to
# reach. `stage:` comes from the guest's own stdout, so the timings are the
# guest's rather than the harness's guesses.
set +e
timeout "$LIMIT" node "$ROOT/shims/browser/js/run-node.mjs" \
    --dir "/=$R" --cwd / --argv0 /bin/sh "$SEED" "$R/bin/sh" /stages.sh 2>&1 |
    awk -v t0="$start" '
        { print; fflush() }
        /^stage:|frames in|Fatal server error|nothing to translate|no such/ {
            cmd = "date +%s"; cmd | getline now; close(cmd)
            printf "     [%4ds] %s\n", now - t0, $0 > "/dev/stderr"
        }' | tee "$out"
rc=$?
set -e
elapsed=$(( $(date +%s) - start ))

if grep -q "$WANT" "$out"; then
    echo "ok   graphics-in-browser($WHAT) in ${elapsed}s"
    rm -f "$out"
    exit 0
fi
echo "FAIL graphics-in-browser($WHAT): never reached \"$WANT\" (${elapsed}s, rc=$rc)"
echo "     last stage reached:"
grep -E "^stage:|Fatal|nothing to translate" "$out" | tail -3 | sed 's/^/       /'
rm -f "$out"
exit 1
