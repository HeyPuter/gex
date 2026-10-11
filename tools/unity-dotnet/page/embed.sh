#!/bin/bash
# embed.sh NAME [browser-linux.mjs args]: tests/unity-dotnet/embed in the page (64-bit memory, the
# Mono provider replacing libmonobdwgc-2.0.so), interpreter only unless args say otherwise.
set -u
cd ~/gex-sandbox/wt/unity && . ~/gex-sandbox/scratch/env.sh
n=$1; shift
D=~/gex-sandbox/scratch/unity/page/embed
mkdir -p ~/gex-sandbox/scratch/unity/page/tmp
rm -f web/public/gex/nt/$n.tar
TMPDIR=$HOME/gex-sandbox/scratch/unity/page/tmp GEX_NT_CHROME_PORT=9531 GEX_NT_VITE_PORT=5391 GEX_GL_X64=$HOME/gex-sandbox/scratch/unity/gl64-new \
  nice -n 10 timeout 900 node tests/games/browser-linux.mjs --name $n --dir $D --exe embed --cwd . --files ~/gex-sandbox/scratch/unity/page/embed-files.txt \
  --args "./libmonobdwgc-2.0.so /game/asm/t.dll" --mem64 --wasmdlls libmonobdwgc-2.0.so=/game/mono.wasm \
  --env "GEXMONO_ASSEMBLIES=/game/lib,GEXMONO_WASM_JIT=${JIT:-0}${EXTRA_ENV:+,$EXTRA_ENV}" --end-on-exit --minutes ${MINUTES:-10} "$@" > ~/gex-sandbox/scratch/unity/logs/page-$n.log 2>&1
grep -v "^\[nt-browser\] console" ~/gex-sandbox/scratch/unity/logs/page-$n.log | tail -25
