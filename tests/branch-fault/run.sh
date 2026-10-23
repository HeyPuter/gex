#!/bin/bash
# A branch into data is SIGSEGV, natively and in the page: the program's
# handler runs, and without one the whole process dies of it (a second
# thread still running) and its parent's wait says so.
#
#   tests/branch-fault/run.sh [gexc]
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
w=${WORK:-$root/target/branch-fault-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O1 -static -o "$w/fault" "$here/fault.c" -lpthread || exit 1
want="handler: signal 11 at data 1
handled: exited 42
threaded: killed by signal 11"
fail=0
check() { # NAME OUT ERR
	if [ "$2" = "$want" ]; then
		echo "ok   branch-fault $1"
	else
		echo "FAIL branch-fault $1"; printf 'got:\n%s\n' "$2"; tail -5 "$3"; fail=1
	fi
}
out=$(timeout 60 "$gexc" run "$w/fault" 2> "$w/native.err")
check native "$out" "$w/native.err"
seed=$root/web/public/gex/seed.shared.wasm
if [ -e "$seed" ]; then
	out=$(cd "$w" && GEX_SHARED=1 timeout 120 node "$root/shims/browser/js/run-node.mjs" \
		--file "$w/fault=$w/fault" "$seed" "$w/fault" 2> "$w/browser.err" | grep -av '^\[gex\]')
	check browser "$out" "$w/browser.err"
else
	echo "skip branch-fault browser ($seed is not there)"
fi
exit $fail
