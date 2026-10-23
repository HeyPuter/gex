#!/bin/bash
# A block whose code runs onto a page translated again since is reached by
# a direct branch from a block of its own translation whose page did not
# change: that edge is stale for good, and every call through it dispatches
# on to the newer translation from inside the guest -- not in a trip out to
# JavaScript and back each time.
#
#   tests/stale-sibling/run.sh [gexc]
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN); the browser
# leg needs the shared build in web/public/gex (GEX_SHARED=1
# shims/browser/build.sh) and is skipped without it.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/release/gexc}")
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
for t in "$gexc" "$cc"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/stale-sibling-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O1 -static -o "$w/sibling" "$here/sibling.c" || exit 1
fail=0
out=$(timeout 60 "$gexc" run "$w/sibling" 2> "$w/native.err")
if [ "$out" = "sum 50000" ]; then echo "ok   stale-sibling native"; else echo "FAIL stale-sibling native: $out"; tail -5 "$w/native.err"; fail=1; fi
seed=$root/web/public/gex/seed.shared.wasm
if [ -e "$seed" ]; then
	# GEX_JIT_DIAG says how often stale blocks dispatched on (thousands
	# of times here) and how often one was resumed from JavaScript from
	# the second time on (it must not be)
	out=$(cd "$w" && GEX_JIT_DIAG=1 GEX_SHARED=1 timeout 120 node "$root/shims/browser/js/run-node.mjs" \
		--file "$w/sibling=$w/sibling" "$seed" "$w/sibling" 2> "$w/browser.err" | grep -av '^\[gex\]')
	again=$(grep -ac 'resumed from JavaScript [0-9]* times' "$w/browser.err")
	onward=$(grep -ac 'dispatched on to a newer translation' "$w/browser.err")
	if [ "$out" = "sum 50000" ] && [ "$again" -eq 0 ] && [ "$onward" -ge 1 ]; then
		echo "ok   stale-sibling browser"
	else
		echo "FAIL stale-sibling browser: $out; resumed again: $again lines; dispatched on: $onward lines"; grep -a 'resumed from JavaScript' "$w/browser.err" | tail -3; fail=1
	fi
else
	echo "skip stale-sibling browser ($seed is not there)"
fi
exit $fail
