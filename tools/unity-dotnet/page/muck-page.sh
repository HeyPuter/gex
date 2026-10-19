#!/bin/bash
# muck-page.sh NAME MINUTES [browser-linux.mjs args]: Muck on the .NET provider in Chromium (edger, real GPU).
# Runs with the function JIT (--fjit; FJIT=0 for without): every Muck page figure since
# 2026-10-08 (mp23 on) is with it, and without it the menu runs at 10.1 against 13.2
# presents/s (edger, 2026-10-11, same tree and guest GL).
set -u
n=$1 m=$2; shift 2
# TREE: the checkout to run (default: the one this script is in). Its guest GL is built from
# its own sources (guest-gl.sh); GL64= overrides it.
HERE=$(cd "$(dirname "$0")" && pwd)
TREE=$(realpath "${TREE:-$HERE/../../..}")
GL64=${GL64:-$(bash "$HERE/guest-gl.sh" "$TREE")}
cd "$TREE"
P=$HOME/gex-sandbox/scratch/unity/page
# BCL: the BCL directory (.NET 10 framework + this build's CoreLib + mono.wasm); FILES: host files
# from a native --strace run. Staging since 2026-10-19: scratch/G2/muck/{bcl,muck-files.txt}.
BCL=${BCL:-$P/muck-bcl} FILES=${FILES:-$P/muck-files.txt}
mkdir -p $P/tmp $P/shots/$n
rm -f web/public/gex/nt/$n.tar
export PATH=$HOME/toolchains/node/bin:$PATH
TMPDIR=$P/tmp GEX_NT_CHROME_PORT=9531 GEX_NT_VITE_PORT=5391 GEX_WEBGPU_SOFTWARE=0 GEX_GL_X64=$GL64 \
  timeout $((m * 60 + 300)) node tests/games/browser-linux.mjs --name $n --dir $HOME/gex-sandbox/games-linux/Muck --exe Muck.x86_64 --cwd . \
  --libs $BCL --files $FILES --mem64 --wasmdlls libmonobdwgc-2.0.so=/gex/libs/0/mono.wasm \
  --env GEXMONO_ASSEMBLIES=/gex/libs/0${PAGE_ENV:+,$PAGE_ENV} --size 800x600 --minutes $m --shot $P/shots/$n \
  --args "-screen-width 800 -screen-height 600 -screen-fullscreen 0 -logFile -" $([ "${FJIT:-1}" = 0 ] || echo --fjit) "$@" > $P/$n.log 2>&1
rc=$?
# Clean up: the bundle (hundreds of MiB) and Chromium's profile.
rm -f web/public/gex/nt/$n.tar; rm -rf $P/tmp/org.chromium.Chromium.*
echo "rc $rc"
grep -v "console\[" $P/$n.log | tail -30
