#!/bin/bash
# gl43-msvo-probe in the page on a real GPU: the compute image shapes a
# screen-space AO pass uses, Unity's MultiScaleVO in particular. Its
# unformatted r16f and r8 cases are what left Muck black from GL 4.3 up.
#
#   tests/gl43-msvo-probe-page.sh --build   on the build host: the x86-64 guest GL
#       (tools/unity-dotnet/page/guest-gl.sh) and the probe against it, into
#       target/guest-gl-x64 and target/msvo-probe (bench hosts lack the GL headers)
#   tests/gl43-msvo-probe-page.sh [browser-linux args]   on the bench host (through
#       harness/bench/bench.sh): runs the probe built above with
#       tests/games/browser-linux.mjs; passes when it reports no failure.
#       QUERY= sets the page query (default gllimits=43, GL 4.6 with compute, whatever
#       the page's default limits are).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PROBE=$ROOT/target/msvo-probe
if [ "${1:-}" = --build ]; then
    GL64=${GL64:-$(bash "$ROOT/tools/unity-dotnet/page/guest-gl.sh" "$ROOT")}
    mkdir -p "$PROBE/game"
    inc=; [ -f "$ROOT/third_party/mesa/include/EGL/egl.h" ] && inc="-I $ROOT/third_party/mesa/include"
    cc -O2 $inc -o "$PROBE/game/msvo" "$ROOT/tests/integration/programs/gl43-msvo-probe.c" \
        -L "$GL64" -l:libEGL.so.1 -l:libGL.so.1 -lm || exit 1
    printf '%s\n' /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libm.so.6 > "$PROBE/files.txt"
    rm -rf "$PROBE/gl64"; cp -a "$GL64" "$PROBE/gl64" || exit 1
    echo "built $PROBE"
    exit 0
fi
[ -x "$PROBE/game/msvo" ] || { echo "FAIL no probe: run $0 --build on the build host first"; exit 1; }
W=$(mktemp -d "${TMPDIR:-/tmp}/msvo-probe.XXXXXX")
mkdir -p "$W/shots" "$W/tmp"
cd "$ROOT"
TMPDIR=$W/tmp GEX_WEBGPU_SOFTWARE=0 GEX_GL_X64=$PROBE/gl64 timeout 400 node tests/games/browser-linux.mjs \
    --name msvo-probe --dir "$PROBE/game" --exe msvo --cwd . --files "$PROBE/files.txt" \
    --query "${QUERY:-gllimits=43}" --size 800x600 --minutes 1 --shot "$W/shots" "$@" > "$W/run.log" 2>&1
rm -f web/public/gex/nt/msvo-probe.tar
grep -a 'msvo:' "$W/run.log"
if grep -aq 'msvo: 0 failure(s)' "$W/run.log"; then
    echo "ok gl43-msvo-probe"; rm -rf "$W"; exit 0
fi
echo "FAIL gl43-msvo-probe (log kept in $W)"
exit 1
