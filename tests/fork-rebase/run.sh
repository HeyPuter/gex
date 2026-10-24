#!/bin/bash
# A forked child adopting its parent's translations keeps the directory it
# inherited: where the parent moved a page to another base after
# translating it, the translation's older base is not written back
# (GEX_TEST_FORK_REBASE moves the first adopted module's pages, as such a
# parent leaves them; GEX_JIT_DIAG says each page given a second base).
#
#   tests/fork-rebase/run.sh [gexc]
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
w=${WORK:-$root/target/fork-rebase-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O1 -static -pthread -o "$w/fork" "$here/fork.c" || exit 1
want="child 299995
parent 299995 2997, child ok"
out=$(cd "$w" && GEX_ADOPT_AFTER=0 GEX_TEST_FORK_REBASE=1 GEX_JIT_DIAG=1 GEX_SHARED=1 timeout 120 node "$root/shims/browser/js/run-node.mjs" \
	--file "$w/fork=$w/fork" "$seed" "$w/fork" 2> "$w/browser.err" | grep -av '^\[gex\]')
moved=$(grep -ac 'by test-fork-rebase' "$w/browser.err")
back=$(grep -ac 're-based .* by register-module' "$w/browser.err")
if [ "$out" = "$want" ] && [ "$moved" -ge 1 ] && [ "$back" -eq 0 ]; then
	echo "ok   fork-rebase browser"
else
	echo "FAIL fork-rebase browser: $out; pages moved: $moved; moved back: $back"; grep -a 're-based' "$w/browser.err" | tail -3
	exit 1
fi
