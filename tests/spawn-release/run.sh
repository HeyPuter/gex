#!/bin/bash
# The page's main thread lets go of a fork child's record once the child's
# Worker has it, though the child runs on: the record names every module the
# parent had (its own and its parents'), and held for each child's life they
# came to some twenty thousand modules on a desktop's page.
# (GEX_TEST_FORK_RECORDS: the host says, half a second after the hand-over
# and a collection, whether the record is still reachable.)
#
#   tests/spawn-release/run.sh
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN) and the
# shared build in web/public/gex (GEX_SHARED=1 shims/browser/build.sh);
# skipped without them.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
seed=$root/web/public/gex/seed.shared.wasm
for t in "$cc" "$seed"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/spawn-release-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O1 -static -o "$w/fork" "$here/fork.c" || exit 1
want="child 2000
parent 400000, child ok"
out=$(cd "$w" && GEX_TEST_FORK_RECORDS=1 GEX_SHARED=1 timeout 120 node --expose-gc "$root/shims/browser/js/run-node.mjs" \
	--file "$w/fork=$w/fork" "$seed" "$w/fork" 2> "$w/browser.err" | grep -av '^\[gex\]')
released=$(grep -ac 'fork record released while it runs' "$w/browser.err")
kept=$(grep -ac 'fork record kept while it runs' "$w/browser.err")
if [ "$out" = "$want" ] && [ "$released" -ge 1 ] && [ "$kept" -eq 0 ]; then
	echo "ok   spawn-release browser"
else
	echo "FAIL spawn-release browser: $out; records released: $released, kept: $kept"; grep -a '\[test\]' "$w/browser.err" | tail -3
	exit 1
fi
