#!/bin/bash
# A fork's staged child belongs to the Worker that made it. Two threads, each
# on its own Worker, fork at once, and each makes detached starts (a vfork
# child that forks and exits) between: a fork staged where another Worker
# could claim it, or overwrite it, lost a child (its parent waited for good)
# or started one on another thread's context (main.c). Every grandchild must
# report, and say its registers survived.
#
#   tests/fork-staged/run.sh
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN) and the shared
# browser build in web/public/gex (shims/browser/build.sh).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
seed=$root/web/public/gex/seed.shared.wasm
for t in "$cc" "$seed"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/fork-staged-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O1 -o "$w/main" "$here/main.c" -lpthread || exit 1
n=20 runs=${RUNS:-4} fail=0
for i in $(seq 1 "$runs"); do
	(cd "$w" && GEX_SHARED=1 timeout "${TMO:-150}" node "$root/shims/browser/js/run-node.mjs" \
		--file "/lib/ld-musl-aarch64.so.1=$tc/aarch64-linux-musl/lib/libc.so" --file "$w/main=$w/main" \
		"$seed" "$w/main" $n 1 > "$w/run$i.out" 2> "$w/run$i.err" < /dev/null); rc=$?
	if [ $rc != 0 ] || ! grep -q "^Y=$((n * 2)) N=0 of $((n * 2))$" "$w/run$i.out"; then
		echo "FAIL fork-staged run $i (rc=$rc): $(cat "$w/run$i.out")"; grep -a 'killed by\|not run in place' "$w/run$i.err" | head -3
		fail=1
	fi
done
[ $fail = 0 ] && echo "ok   fork-staged ($runs runs, $((n * 2)) detached starts each)"
exit $fail
