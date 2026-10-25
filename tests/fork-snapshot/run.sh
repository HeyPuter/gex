#!/bin/bash
# A fork's child gets the parent's memory as it was at one moment, as Linux
# copies it, while the parent's other threads run on (main.c): threads write
# B then A, far apart in memory, and the child must see them as some moment
# left them. Copied a region at a time while those threads ran, the child's
# memory had A from one moment and B from a later one, every fork; glibc's
# child walked its stack lists as no thread ever left them and crashed.
#
#   tests/fork-snapshot/run.sh
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
w=${WORK:-$root/target/fork-snapshot-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O1 -o "$w/main" "$here/main.c" -lpthread || exit 1
forks=${FORKS:-20} threads=${THREADS:-4}
(cd "$w" && GEX_SHARED=1 timeout "${TMO:-300}" node "$root/shims/browser/js/run-node.mjs" \
	--file "/lib/ld-musl-aarch64.so.1=$tc/aarch64-linux-musl/lib/libc.so" --file "$w/main=$w/main" \
	"$seed" "$w/main" "$forks" "$threads" > "$w/run.out" 2> "$w/run.err" < /dev/null); rc=$?
if [ $rc = 0 ] && grep -q "^forks $forks torn 0$" "$w/run.out"; then
	echo "ok   fork-snapshot ($forks forks beside $threads writing threads, each child one moment's memory)"
else
	echo "FAIL fork-snapshot (rc=$rc): $(tail -1 "$w/run.out")"; grep -a '^child' "$w/run.out" | head -3
	exit 1
fi
