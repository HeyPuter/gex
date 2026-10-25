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

# A page leg's view of a game's memory (peek.c): `?memories=1` (here
# GEX_MEMORIES=1) registers a threaded process's memory for the harness, and
# with the fork record let go that registry is all that must keep it. Read
# after a collection twice a second (GEX_TEST_MEMORIES), the child's counter
# must be there and climbing while it runs, and let go once it has exited;
# without the switch nothing is registered.
"$cc" -O1 -static -no-pie -o "$w/peek" "$here/peek.c" -lpthread || exit 1
at=0x$("${cc%gcc}nm" "$w/peek" | awk '/ counter$/ {print $1}')
peek() { # LOG [env...]
	log=$1; shift
	(cd "$w" && env "$@" GEX_TEST_MEMORIES="$at" GEX_SHARED=1 timeout 120 node --expose-gc "$root/shims/browser/js/run-node.mjs" \
		--file "$w/peek=$w/peek" "$seed" "$w/peek" 2> "$log" | grep -av '^\[gex\]')
}
out=$(peek "$w/peek.err" GEX_MEMORIES=1)
words=$(grep -a '\[test\] memories: pid' "$w/peek.err" | sed 's/.* word //')
climbs=$(awk 'NR > 1 && $1 > p {n++} {p = $1} END {print n + 0}' <<< "$words")
left=$(grep -a '\[test\] memories at exit' "$w/peek.err" | sed 's/.*: //')
if [ "$out" = "child counted 200
child ok" ] && [ "$climbs" -ge 3 ] && [ "$left" = 0 ]; then
	echo "ok   spawn-release memories (the counter read $climbs times higher while the child ran, then let go)"
else
	echo "FAIL spawn-release memories: $out; readings climbed $climbs times; held at exit: ${left:-?}"; exit 1
fi
out=$(peek "$w/peek-off.err")
if grep -aq '\[test\] memories: pid' "$w/peek-off.err"; then
	echo "FAIL spawn-release memories: registered without the switch"; exit 1
fi
echo "ok   spawn-release memories off (nothing registered)"
