#!/bin/bash
# A vfork child may spawn (posix_spawn, CLONE_VM | CLONE_VFORK), natively
# and in the page: Qt's startDetached does, and KDE starts its services so.
#
#   tests/vfork-spawn/run.sh [gexc]
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN); the browser
# legs need the shared build in web/public/gex (GEX_SHARED=1
# shims/browser/build.sh) and are skipped without it. GEXCC, gexcc's
# wasm32-unknown-linux-musl-gcc, adds a wasm32 program's browser leg.
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
# exit 102: posix_spawn's ENOENT (100 + 2); 2: ENOENT from the memory the
# grandchild shared
want="spawn: exited 0
grandchild ran
spawn missing: exited 102
shared: exited 0
grandchild ran
shared missing: exited 2"
fail=0
check() { # NAME OUT ERR [WANT]
	if [ "$2" = "${4:-$want}" ]; then
		echo "ok   vfork-spawn $1"
	else
		echo "FAIL vfork-spawn $1"; printf 'got:\n%s\n' "$2"; tail -5 "$3"; fail=1
	fi
}
# its own root: the grandchild execs /proc/self/exe, a guest path. The
# pipe cases alone: `gexc run` copies a CLONE_VM | CLONE_VFORK child as a
# fork, so what the grandchild writes before it exits never reaches its
# parent (and the parent then waits on the pipe for a grandchild that never
# ran).
out=$(timeout 120 "$gexc" run --sysroot "$w" "$w/spawn" pipe 2> "$w/native.err")
check native "$out" "$w/native.err" "$(printf '%s\n' "$want" | head -3)"
seed=$root/web/public/gex/seed.shared.wasm
if [ -e "$seed" ]; then
	out=$(cd "$w" && GEX_SHARED=1 timeout 120 node "$root/shims/browser/js/run-node.mjs" \
		--file "$w/spawn=$w/spawn" "$seed" "$w/spawn" 2> "$w/browser.err" | grep -av '^\[gex\]')
	check browser "$out" "$w/browser.err"
	if [ -n "${GEXCC:-}" ]; then
		"$GEXCC" -O1 -o "$w/spawn32" "$here/spawn.c" || exit 1
		out=$(cd "$w" && GEX_SHARED=1 timeout 120 node "$root/shims/browser/js/run-node.mjs" \
			--file "$w/spawn32=$w/spawn32" "$seed" "$w/spawn32" 2> "$w/wasm32.err" | grep -av '^\[gex\]')
		check "browser wasm32" "$out" "$w/wasm32.err"
	fi
else
	echo "skip vfork-spawn browser ($seed is not there)"
fi
exit $fail
