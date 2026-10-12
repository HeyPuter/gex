#!/bin/bash
# hk-page.sh NAME MINUTES [browser-linux.mjs args]: Hollow Knight on the .NET provider in Chromium
# (edger, real GPU). Staging: scratch/unity/page/hk (hk-stage.py from a native --strace run).
# HK runs on the interpreter by default (q81: 15.6 presents/s against 13.8 under the JIT); JIT=1 for
# the .NET wasm JIT. PAGE_ENV adds guest environment (comma-separated).
set -u
n=$1 m=$2; shift 2
# TREE: the checkout to run (default: the one this script is in). Its guest GL is built from
# its own sources (guest-gl.sh); GL64= overrides it.
HERE=$(cd "$(dirname "$0")" && pwd)
TREE=$(realpath "${TREE:-$HERE/../../..}")
GL64=${GL64:-$(bash "$HERE/guest-gl.sh" "$TREE")}
cd "$TREE"
P=$HOME/gex-sandbox/scratch/unity/page
mkdir -p $P/tmp $P/shots/$n
rm -f web/public/gex/nt/$n.tar
export PATH=$HOME/toolchains/node/bin:$PATH
TMPDIR=$P/tmp GEX_NT_CHROME_PORT=9531 GEX_NT_VITE_PORT=5391 GEX_WEBGPU_SOFTWARE=0 GEX_GL_X64=$GL64 \
  timeout $((m * 60 + 300)) node tests/games/browser-linux.mjs --name $n --dir $P/hk/game --exe hollow_knight.x86_64 --cwd . \
  --libs ${BCL:-$P/muck-bcl} --files $P/hk/files.txt --home $TREE/tests/games/seeds/hollow-knight \
  --mem64 --wasmdlls libmonobdwgc-2.0.so=/gex/libs/0/mono.wasm \
  --env GEXMONO_ASSEMBLIES=/gex/libs/0,GEXMONO_WASM_JIT=${JIT:-0}${PAGE_ENV:+,$PAGE_ENV} --size 800x600 --minutes $m --shot $P/shots/$n \
  --args "-screen-width 800 -screen-height 600 -screen-fullscreen 0 -logFile -" "$@" > $P/$n.log 2>&1
rc=$?
# Clean up: the bundle (hundreds of MiB) and Chromium's profile.
rm -f web/public/gex/nt/$n.tar; rm -rf $P/tmp/org.chromium.Chromium.*
echo "rc $rc"
grep -v "console\[" $P/$n.log | tail -30
