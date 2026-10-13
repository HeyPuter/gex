#!/bin/bash
# Noita page leg (gate): Noita on W3 (Wine's own i386 Unix side) in headless
# Chromium on a bench box (BENCH_HOST, default edger; games never run on
# gooner), from TREE's x86 shim and kernel against frozen Wine/game bundles
# and function-tier units. tests/games/noita-page.mjs drives it: the canonical
# recipe's clicks, a pinned world seed, then the world's frame counter
# (noita.exe 0x122374c) over LEG_SECS (300) seconds of gameplay.
#
#   tests/games/noita-page-leg.sh TREE [--no-build]
#
# PASS: the world comes up, keeps running (no 30 s without a world frame), and
# runs at LEG_MIN_WFPS world frames/s or more (default below, from baseline
# runs on edger). FAIL also when noita.exe has no function-tier unit: it would
# run on tier-1, and the number would mean nothing.
#
# The frozen inputs, in NOITA_LEG_DIR (default ~/gex-sandbox/scratch/W/gate-noitapage):
#   base/   a W3 page tree (W's tree C, 2026-10-10: Mesa, node_modules), never
#           written in place, with web/public/gex/nt/w3d.tar, the W3 dist: tree
#           C's, with the PE skeletons, winepe.wasm and wine{,.wasm} rebuilt from
#           a wine-w3 commit and wine linked against a gex tree's gexgl.c
#           (base/README.base gives the commits and the recipe; for gexgl alone,
#           scratch/W/gate-noitapage/w3d-gexgl.sh), plus
#           web/public/gex/nt/noita.tar: the W3 prefix at /pfx with
#           port/seedpin's mod_config.xml in its save, xkb data, and the game at
#           /g with mods/seedpin (NOITA_WORLD_SEED 1767225600), made by
#           `noita-page-leg.sh --make-tar` from ~/gex-sandbox/Noita;
#   units/  the function-tier units of every image of the run.
# UNITS MUST BE REBUILT WHENEVER crates/gex-cgen CHANGES (the keys include
# its sources): `GEX_CGEN_CACHE=units gexc cgen-image IMAGE --pe-base BASE`
# with TREE's x86 gexc for each "unitless:" line of a run (page paths: /w3 is
# the W3 dist, /g the game). A stale cache shows up as "unitless" lines.
#
# One bench.sh call per run, timed class (BENCH_TIMEOUT 20m). Takes about
# 9 min to build on gooner, then the run.
set -u
if [ "${1:-}" = --remote ]; then
  # The bench box's half: TREE NOITA_EXE UNITS SECS, from the sandbox root.
  T=$(cd $2 && pwd) X=$(realpath $3) U=$(cd $4 && pwd) secs=$5
  free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
  CP=$(free_port); VP=$(free_port)
  mkdir -p $T/../tmp
  export TMPDIR=$(cd $T/.. && pwd)/tmp GEX_NT_CHROME_PORT=$CP GEX_NT_VITE_PORT=$VP GEX_WEBGPU_SOFTWARE=0
  export GEX_CGEN_CACHE=$U GEX_NT_BROWSER_KEEP="no function-tier unit|validation${NOITA_KEEP:+|$NOITA_KEEP}"
  kill_ours() { for p in $(pgrep -f -- "--remote-debugging-port=$CP\b"); do kill $p 2>/dev/null; done; }
  trap 'kill_ours; [ -n "${np:-}" ] && kill $np 2>/dev/null; exit 143' TERM INT
  node $T/tests/games/noita-page.mjs $T $X $secs 2> $TMPDIR/noita-err.txt & np=$!
  wait $np; rc=$?
  kill_ours
  exit $rc
fi
A=${NOITA_LEG_DIR:-$HOME/gex-sandbox/scratch/W/gate-noitapage}
if [ "${1:-}" = --make-tar ]; then
  # noita.tar from ut.tar's prefix and xkb data and the pinned game.
  S=$A/stage; rm -rf $S; mkdir -p $S
  tar -C $S -xf $A/base/web/public/gex/nt/ut.tar pfx usr
  sh $HOME/gex-sandbox/port/seedpin/pin.sh farm $S/farm
  mkdir -p $S/g && cp -rL $S/farm/. $S/g/ && rm -rf $S/farm
  rm -rf $S/g/tools_modding $S/g/noita_dev.exe $S/g/crashes $S/g/logger.txt
  # The save's mod list, for the profile Wine makes: under the page's user,
  # and under an empty one (users/AppData), which a page prefix has had.
  for d in $S/pfx/drive_c/users/gex/AppData $S/pfx/drive_c/users/AppData; do
    d=$d/LocalLow/Nolla_Games_Noita/save00
    rm -rf "$d"; mkdir -p "$d"; cp $HOME/gex-sandbox/port/seedpin/mod_config.xml "$d/"
  done
  tar -C $S -cf $A/noita.tar.new pfx usr g && mv $A/noita.tar.new $A/base/web/public/gex/nt/noita.tar
  rm -rf $S; ls -la $A/base/web/public/gex/nt/noita.tar; exit 0
fi
W=$(cd "${1:?usage: noita-page-leg.sh TREE [--no-build]}" && pwd)
. ~/gex-sandbox/scratch/env.sh 2>/dev/null
B=~/gex-sandbox/scratch/bench/bench.sh
# Baselines (edger, 300 s windows, run alternately against master):
# step/klock-9 7ee38ef2 (the poll family lock-free) 13.1, 16.3 and 16.2
# world frames/s, the world up at 74-78 s; master 3.2 and 3.4 (2026-10-12,
# master 3a61c3d8: 3.6, 3.6, 3.7). The floor is about 23% under klock-9's
# 16.2 median, leaving room for a cold first run like the 13.1.
MIN=${LEG_MIN_WFPS:-12.5}
if [ "${2:-}" != "--no-build" ]; then
  mkdir -p $W/web/public/gex
  for s in kernel browser; do
    if ! (cd $W && GEX_ARCH=x86 systemd-run --user --scope -q -p TasksMax=4096 -p MemoryMax=32G nice -n 19 sh shims/$s/build.sh > $A/build-$s.log 2>&1); then
      echo "FAIL noita page: x86 $s shim build failed (tail: $(tail -3 $A/build-$s.log | tr '\n' ' '))"; exit 1
    fi
  done
fi
[ -f $A/base/web/public/gex/nt/noita.tar ] || { echo "FAIL noita page: no $A/base/web/public/gex/nt/noita.tar (--make-tar)"; exit 1; }
# The run tree, at one path so the bench box's copy updates by difference.
T=$A/tree; rm -rf $T; cp -al $A/base $T
for d in crates shims web/src web/index.html web/nt.html tests tools; do
  [ -e $W/$d ] || continue
  rm -rf $T/$d
  (cd $W && git ls-files -z $d | xargs -0 -I{} cp --parents {} $T/)
done
for f in gex_browser.x86.shared.wasm gex_browser.x86.wasm gex_kernel.x86.plain.wasm gex_kernel.x86.wasm seed.x86.shared.wasm seed.x86.wasm; do
  [ -e $W/web/public/gex/$f ] || { echo "FAIL noita page: $W/web/public/gex/$f missing"; exit 1; }
  rm -f $T/web/public/gex/$f; cp $W/web/public/gex/$f $T/web/public/gex/$f
done
R=${A#$HOME/gex-sandbox/}
# NOITA_SHOTS=1: screenshots of each click and every 30 s, back into $A/shots
# (edger only).
# WDBG: Wine's debug channels for the run (noita-page.mjs's default otherwise).
shots="env NOITA_WORLD_BY_MIN=${NOITA_WORLD_BY_MIN:-12}"
[ -n "${NOITA_SHOTS:-}" ] && shots="$shots NOITA_SHOT=$HOME/gex-sandbox/$R/shots"
[ -n "${WDBG:-}" ] && shots="$shots WDBG=$WDBG"
# NOITA_PROF=AT,MS (a profiling run, see noita-page.mjs), NOITA_Q (the page's
# query flags, default &cgen=1), NOITA_KEEP (console lines to keep).
[ -n "${NOITA_PROF:-}" ] && shots="$shots NOITA_PROF=$NOITA_PROF"
[ -n "${NOITA_Q:-}" ] && shots="$shots NOITA_Q=$NOITA_Q"
[ -n "${NOITA_KEEP:-}" ] && shots="$shots NOITA_KEEP=$NOITA_KEEP"
BENCH_TIMEOUT=${BENCH_TIMEOUT:-20m} $B -s $T -s $A/units -- $shots bash $R/tree/tests/games/noita-page-leg.sh --remote $R/tree Noita/noita.exe $R/units ${LEG_SECS:-300} > $A/last.txt 2>&1
[ -n "${NOITA_SHOTS:-}" ] && { rm -rf $A/shots; rsync -a claude@edger:gex-sandbox/$R/shots/ $A/shots/; ssh claude@edger "rm -rf gex-sandbox/$R/shots"; }
res=$(grep -m1 '^== noita' $A/last.txt)
if grep -q '^unitless: .*/noita\.exe ' $A/last.txt; then echo "FAIL noita page: noita.exe has no function-tier unit (rebuild $A/units for this gex-cgen); ${res:-no result}"; exit 1; fi
wfps=$(echo "$res" | sed -n 's/.*, world \([0-9.]*\) frames\/s.*/\1/p')
if [ -z "$wfps" ]; then echo "FAIL noita page: ${res:-no result}; log $A/last.txt"; exit 1; fi
if ! awk -v a="$wfps" -v b="$MIN" 'BEGIN { exit !(a + 0 >= b + 0) }'; then echo "FAIL noita page: world $wfps frames/s, want >= $MIN; ${res#== noita: }"; exit 1; fi
echo "ok   noita page (${res#== noita: })"; exit 0
