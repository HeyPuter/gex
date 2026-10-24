#!/bin/bash
# /proc's threads as Linux has them: tasks.c, built for this machine and run
# here, then built for the guest and run on gex, must print the same (ids
# named, not numbered). The guest runs under gexc and, when the shared
# browser build is there, under run-node.mjs, where the process table is
# the kernel module's and /proc reads it through its snapshot.
#
#   tests/proc-tasks/run.sh [gexc]
#
# Needs a host cc and ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/release/gexc}")
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
seed=$root/web/public/gex/seed.shared.wasm
for t in "$gexc" "$cc"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/proc-tasks-test}
rm -rf "$w"; mkdir -p "$w"
${HOSTCC:-cc} -O1 -pthread -o "$w/native" "$here/tasks.c" || exit 1
"$cc" -O1 -static -pthread -o "$w/tasks" "$here/tasks.c" || exit 1
"$w/native" > "$w/linux.out" 2>&1 || { echo "FAIL proc-tasks: the Linux run failed"; cat "$w/linux.out"; exit 1; }
fail=0
check() { # NAME OUT
	if diff -u "$w/linux.out" "$2" > "$w/$1.diff"; then
		echo "ok   proc-tasks $1 (prints what Linux does)"
	else
		echo "FAIL proc-tasks $1"; cat "$w/$1.diff"; fail=1
	fi
}
timeout "${TMO:-120}" "$gexc" run "$w/tasks" < /dev/null > "$w/gexc.out" 2> "$w/gexc.err"
check gexc "$w/gexc.out"
if [ -e "$seed" ]; then
	(cd "$w" && GEX_SHARED=1 timeout "${TMO:-300}" node "$root/shims/browser/js/run-node.mjs" \
		--file "$w/tasks=$w/tasks" "$seed" "$w/tasks" < /dev/null > "$w/node.out" 2> "$w/node.err")
	grep -av '^\[' "$w/node.out" > "$w/node.clean"
	check run-node "$w/node.clean"
else
	echo "skip proc-tasks run-node: no shared browser build in web/public/gex"
fi
exit $fail
