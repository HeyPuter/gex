#!/bin/bash
# A block whose code runs onto a page translated again since is reached by
# a direct branch from a block of its own translation whose page did not
# change: that edge is stale for good, and the translation it belongs to is
# retired at the first such entry, so the program goes on in translations
# made since -- not in a trip out of the guest and back on every call.
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
if [ "$out" = "sum 5000" ]; then echo "ok   stale-sibling native"; else echo "FAIL stale-sibling native: $out"; tail -5 "$w/native.err"; fail=1; fi
seed=$root/web/public/gex/seed.shared.wasm
if [ -e "$seed" ]; then
	# GEX_JIT_DIAG says when the stale translation is retired (it must
	# be) and how often one stale block is resumed from JavaScript from
	# the second time on (it must not be)
	out=$(cd "$w" && GEX_JIT_DIAG=1 GEX_SHARED=1 timeout 120 node "$root/shims/browser/js/run-node.mjs" \
		--file "$w/sibling=$w/sibling" "$seed" "$w/sibling" 2> "$w/browser.err" | grep -av '^\[gex\]')
	again=$(grep -ac 'resumed from JavaScript [0-9]* times' "$w/browser.err")
	retired=$(grep -ac 'reached by a direct branch' "$w/browser.err")
	if [ "$out" = "sum 5000" ] && [ "$again" -eq 0 ] && [ "$retired" -ge 1 ]; then
		echo "ok   stale-sibling browser"
	else
		echo "FAIL stale-sibling browser: $out; resumed again: $again lines; retired: $retired lines"; grep -a 'resumed from JavaScript' "$w/browser.err" | tail -3; fail=1
	fi
else
	echo "skip stale-sibling browser ($seed is not there)"
fi
exit $fail
