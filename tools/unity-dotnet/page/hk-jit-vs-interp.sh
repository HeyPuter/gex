#!/bin/bash
# Provider p31 = p30 + 0022 (integer/conversion ops read and write at the register width: ReadFloat; V8 validation
# failures are final). hg26 JIT profile, hg24 interpreter profile, hg27 JIT counters + VERBOSE=1, then cb5/cb6.
# BENCH_TIMEOUT bounds each run inside the lock.
cd ~/gex-sandbox
W=wt/unity-rk; E=claude@edger:gex-sandbox/wt/unity
rsync -a scratch/unity/shim-m4/ $E/web/public/gex/
rsync -a $W/web/src/ $E/web/src/
rsync -a $W/shims/browser/js/ $E/shims/browser/js/
rsync -a $W/tests/games/ $E/tests/games/
rsync -a $W/tests/nt/ $E/tests/nt/
U=/home/claude/gex-sandbox/scratch/unity/units/cgen-page-m1
LIBS=libc.so.6,libX11.so.6,libdbus-1.so.3,libudev.so.1,libasound.so.2,libcrypto.so.3,libpulsecommon-17.0.so,libpulse.so.0,libm.so.6,libxcb.so.1,libpulse-simple.so.0,libXrandr.so.2,libXi.so.6,libsystemd.so.0
Q="cgen=1&elfunits=UnityPlayer.so,$LIBS&pins=UnityPlayer.so@11752000%2B2674000&fjitlibs=byobj=1"
ST=MONO_WASM_JIT_STATS=1,MONO_WASM_JIT_STATS_EVERY=60,MONO_WASM_JIT_STATS_TOPN=150,MONO_WASM_JIT_VERBOSE=1
for run in "hg26 p31 - 700,20,/worker-" "hg24 p31 GEXMONO_WASM_JIT=0 700,20,/worker-" "hg27 p31 $ST -"; do
  set -- $run
  B=/home/claude/gex-sandbox/scratch/unity/page/muck-bcl-$2
  PE=$3; [ "$PE" = - ] && PE=
  PR=(); [ "$4" != - ] && PR=(--profile $4)
  BENCH_TIMEOUT=50m scratch/bench/bench.sh -- env GEX_NT_PROFILE_FIRST=1 GEX_NT_PROFILE_US=1000 GEX_CGEN_CACHE=$U GL64=/home/claude/gex-sandbox/scratch/unity/gl64q2 \
    BCL=$B PAGE_ENV=$PE bash scratch/unity/page/hk-page.sh $1 12 --fjit --query "$Q" \
    --lazy "/home/claude/gex-sandbox/games-linux/Hollow Knight" --input tests/games/hollow-knight-page-input.txt "${PR[@]}" > scratch/unity/eruns/$1.out 2>&1
  mkdir -p scratch/unity/page/shots/$1; rsync -a claude@edger:gex-sandbox/scratch/unity/page/shots/$1/ scratch/unity/page/shots/$1/
  echo $1 > scratch/unity/eruns/q81.$1.done
done
PE=claude@edger:gex-sandbox/scratch/unity/page/embed
rsync -a scratch/unity/dnc-gc/provider/mono.wasm.p31 $PE/mono.wasm
for run in "cb5 0" "cb6 1"; do
  set -- $run
  for rep in 1 2; do
    BENCH_TIMEOUT=20m scratch/bench/bench.sh -- env JIT=$2 EXTRA_ENV=EMBED_BENCH=20000000,MONO_WASM_JIT_THRESHOLD=1 MINUTES=8 \
      bash scratch/unity/page/embed.sh $1-$rep > scratch/unity/eruns/$1-$rep.out 2>&1
  done
done
echo done > scratch/unity/eruns/q81.done
