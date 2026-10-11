#!/bin/bash
# muck-page.sh NAME MINUTES [browser-linux.mjs args]: Muck on the .NET provider in Chromium (edger, real GPU).
set -u
n=$1 m=$2; shift 2
cd ~/gex-sandbox/wt/unity
P=$HOME/gex-sandbox/scratch/unity/page
mkdir -p $P/tmp $P/shots/$n
rm -f web/public/gex/nt/$n.tar
export PATH=$HOME/toolchains/node/bin:$PATH
TMPDIR=$P/tmp GEX_NT_CHROME_PORT=9531 GEX_NT_VITE_PORT=5391 GEX_WEBGPU_SOFTWARE=0 GEX_GL_X64=${GL64:-$HOME/gex-sandbox/scratch/unity/gl64-new} \
  timeout $((m * 60 + 300)) node tests/games/browser-linux.mjs --name $n --dir $HOME/gex-sandbox/games-linux/Muck --exe Muck.x86_64 --cwd . \
  --libs $P/muck-bcl --files $P/muck-files.txt --mem64 --wasmdlls libmonobdwgc-2.0.so=/gex/libs/0/mono.wasm \
  --env GEXMONO_ASSEMBLIES=/gex/libs/0${PAGE_ENV:+,$PAGE_ENV} --size 800x600 --minutes $m --shot $P/shots/$n \
  --args "-screen-width 800 -screen-height 600 -screen-fullscreen 0 -logFile -" "$@" > $P/$n.log 2>&1
rc=$?
# Clean up: the bundle (hundreds of MiB) and Chromium's profile.
rm -f web/public/gex/nt/$n.tar; rm -rf $P/tmp/org.chromium.Chromium.*
echo "rc $rc"
grep -v "console\[" $P/$n.log | tail -30
