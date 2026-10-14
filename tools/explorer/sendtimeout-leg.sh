#!/bin/bash
# The sent-message timeout leg: sendtimeout.exe (sendtimeout.c) checks that
# a message another process sent with SendMessageTimeout still reaches a
# receiver that was busy until the sender's timeout ran out, as on Windows.
# Runs twice: RUNS (default 2). Correctness only (edger: BENCH_CLASS=check).
# usage: sendtimeout-leg.sh [TAG]
#   env: GEXC, DIST, PFX, OUT, EXE (a prebuilt sendtimeout.exe), RUNS, SECS (per run, default 180)
set -u
. ~/gex-sandbox/scratch/env.sh
here=$(cd "$(dirname "$0")" && pwd)
tag=${1:-sendtimeout}
X=~/gex-sandbox/scratch/X
G=${GEXC:-$here/../../target/x86/release/gexc}
DIST=${DIST:-$X/dist-dev}; PFX=${PFX:-$X/pfx-mb}; OUT=${OUT:-$X/runs}
out=$OUT/$tag; pfx=$OUT/pfx-$tag
rm -rf "$out" "$pfx"; mkdir -p "$out"
cp -a "$PFX" "$pfx"
if [ -n "${EXE:-}" ]; then cp "$EXE" "$pfx/drive_c/sendtimeout.exe" || exit 1
else TMPDIR=${TMPDIR:-$X/tmp} i686-w64-mingw32-gcc -O2 -o "$pfx/drive_c/sendtimeout.exe" "$here/sendtimeout.c" || exit 1; fi
cd "$pfx/drive_c"
pass=0 runs=${RUNS:-2}
for i in $(seq $runs); do
  timeout -k 10 ${SECS:-180} systemd-run --user --scope -q -p TasksMax=256 -p MemoryMax=32G \
    ${BENCH_CORES:+taskset -c $BENCH_CORES} nice -n 10 "$G" run \
    --env WINEPREFIX="$pfx" --env WINEDEBUG=${WDBG:-err+all,fixme-all} \
    --wayland "$out/wl$i" --wayland-out null \
    "$DIST/lib/wine/i386-unix/wine" 'C:\sendtimeout.exe' > "$out/log$i" 2>&1
  r=$(grep -a "^sendtimeout:" "$out/log$i" | tail -1 | tr -d "\r")
  echo "run $i: ${r:-no result (see $out/log$i)}"
  case $r in *PASS) pass=$((pass + 1));; esac
done
echo "sendtimeout leg: $([ $pass = $runs ] && echo PASS || echo "FAIL ($pass/$runs)")"
[ $pass = $runs ]
