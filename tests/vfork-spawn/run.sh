#!/bin/bash
# A vfork child may spawn (posix_spawn, CLONE_VM | CLONE_VFORK), natively
# and in the page: Qt's startDetached does, and KDE starts its services so.
#
#   tests/vfork-spawn/run.sh [gexc]
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
w=${WORK:-$root/target/vfork-spawn-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O1 -static -o "$w/spawn" "$here/spawn.c" || exit 1
want="child: exited 0
grandchild ran"
fail=0
check() { # NAME OUT ERR
	if [ "$2" = "$want" ]; then
		echo "ok   vfork-spawn $1"
	else
		echo "FAIL vfork-spawn $1"; printf 'got:\n%s\n' "$2"; tail -5 "$3"; fail=1
	fi
}
# its own root: the grandchild execs /proc/self/exe, a guest path
out=$(timeout 120 "$gexc" run --sysroot "$w" "$w/spawn" 2> "$w/native.err")
check native "$out" "$w/native.err"
seed=$root/web/public/gex/seed.shared.wasm
if [ -e "$seed" ]; then
	out=$(cd "$w" && GEX_SHARED=1 timeout 120 node "$root/shims/browser/js/run-node.mjs" \
		--file "$w/spawn=$w/spawn" "$seed" "$w/spawn" 2> "$w/browser.err" | grep -av '^\[gex\]')
	check browser "$out" "$w/browser.err"
else
	echo "skip vfork-spawn browser ($seed is not there)"
fi
exit $fail
