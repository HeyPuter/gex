#!/bin/bash
# gl43-msvo-probe in the page on a real GPU (run it on the bench host, through
# harness/bench/bench.sh): the compute image shapes a screen-space AO pass
# uses, Unity's MultiScaleVO in particular. Its unformatted r16f and r8 cases
# are what left Muck black from GL 4.3 up. Builds the probe for x86_64 against
# this tree's guest GL (tools/unity-dotnet/page/guest-gl.sh, or GL64=), runs it
# with tests/games/browser-linux.mjs, and passes when it reports no failure.
#   tests/gl43-msvo-probe-page.sh [browser-linux args]   (QUERY= adds page query, default gllimits=43)
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
GL64=${GL64:-$(bash "$ROOT/tools/unity-dotnet/page/guest-gl.sh" "$ROOT")}
W=$(mktemp -d "${TMPDIR:-/tmp}/msvo-probe.XXXXXX")
mkdir -p "$W/game" "$W/shots" "$W/tmp"
cc -O2 -o "$W/game/msvo" "$ROOT/tests/integration/programs/gl43-msvo-probe.c" \
    -L "$GL64" -l:libEGL.so.1 -l:libGL.so.1 -lm || exit 1
printf '%s\n' /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libm.so.6 > "$W/files.txt"
cd "$ROOT"
TMPDIR=$W/tmp GEX_WEBGPU_SOFTWARE=0 GEX_GL_X64=$GL64 timeout 400 node tests/games/browser-linux.mjs \
    --name msvo-probe --dir "$W/game" --exe msvo --cwd . --files "$W/files.txt" \
    --query "${QUERY:-gllimits=43}" --size 800x600 --minutes 1 --shot "$W/shots" "$@" > "$W/run.log" 2>&1
rm -f web/public/gex/nt/msvo-probe.tar
grep -a 'msvo:' "$W/run.log"
grep -aq 'msvo: 0 failure(s)' "$W/run.log"
rc=$?
[ $rc = 0 ] && rm -rf "$W" || echo "kept $W"
exit $rc
