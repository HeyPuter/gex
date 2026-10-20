#!/bin/bash
# gl44-persistent-probe in the page on a real GPU: Mesa's immediate-mode
# vertex buffer, a 512 KiB persistent coherent mapping once ARB_buffer_storage
# is on (GL 4.4+), drawn through with a submit after each quad and filled past
# its end. gex_wgpu sends such a mapping to the device in changed 4 KiB blocks
# at each submit; this checks the blocks it sends and the ones it skips.
#
#   tests/gl44-persistent-probe-page.sh --build   on the build host: the x86-64
#       guest GL (tools/unity-dotnet/page/guest-gl.sh) and the probe against it,
#       into target/guest-gl-x64 and target/persistent-probe (bench hosts lack
#       the GL headers)
#   tests/gl44-persistent-probe-page.sh [browser-linux args]   on the bench host
#       (through harness/bench/bench.sh): runs the probe built above with
#       tests/games/browser-linux.mjs; passes when it reports no failure.
#       QUERY= sets the page query (default gllimits=43, GL 4.6).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PROBE=$ROOT/target/persistent-probe
if [ "${1:-}" = --build ]; then
    GL64=${GL64:-$(bash "$ROOT/tools/unity-dotnet/page/guest-gl.sh" "$ROOT")}
    mkdir -p "$PROBE/game"
    inc=; [ -f "$ROOT/third_party/mesa/include/EGL/egl.h" ] && inc="-I $ROOT/third_party/mesa/include"
    cc -O2 $inc -o "$PROBE/game/persist" "$ROOT/tests/integration/programs/gl44-persistent-probe.c" \
        -L "$GL64" -l:libEGL.so.1 -l:libGL.so.1 || exit 1
    printf '%s\n' /lib/x86_64-linux-gnu/libc.so.6 > "$PROBE/files.txt"
    rm -rf "$PROBE/gl64"; cp -a "$GL64" "$PROBE/gl64" || exit 1
    echo "built $PROBE"
    exit 0
fi
[ -x "$PROBE/game/persist" ] || { echo "FAIL no probe: run $0 --build on the build host first"; exit 1; }
W=$(mktemp -d "${TMPDIR:-/tmp}/persistent-probe.XXXXXX")
mkdir -p "$W/shots" "$W/tmp"
cd "$ROOT"
TMPDIR=$W/tmp GEX_WEBGPU_SOFTWARE=0 GEX_GL_X64=$PROBE/gl64 timeout 400 node tests/games/browser-linux.mjs \
    --name persistent-probe --dir "$PROBE/game" --exe persist --cwd . --files "$PROBE/files.txt" \
    --query "${QUERY:-gllimits=43}" --size 800x600 --minutes 1 --shot "$W/shots" "$@" > "$W/run.log" 2>&1
rm -f web/public/gex/nt/persistent-probe.tar
grep -a 'persist:' "$W/run.log"
if grep -aq 'persist: 0 failure(s)' "$W/run.log"; then
    echo "ok gl44-persistent-probe"; rm -rf "$W"; exit 0
fi
echo "FAIL gl44-persistent-probe (log kept in $W)"
exit 1
