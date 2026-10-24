#!/bin/bash
# Function-tier units instantiated in a Worker at their first call, not when
# it catches up (shims/browser/js/gex.mjs `#cgenDefer`, main.c): a thread's
# Worker instantiates libd.so's unit for its constructor, and libb.so's from
# inside that call; b_two, rewritten before, runs its new code there. Runs under run-node.mjs
# (the SAB build, aarch64) deferred and with GEX_UNITS_EAGER=1; both must
# print the same, and the deferred run must instantiate units in the
# thread's Worker at their first call.
#
#   tests/lazy-units/run.sh [gexc]
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN) and the shared
# browser build in web/public/gex (shims/browser/build.sh with GEX_SHARED=1).
set -u
# The "[unit]" lines are debug output.
export GEX_LOG=${GEX_LOG:-unit}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/release/gexc}")
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
seed=$root/web/public/gex/seed.shared.wasm
for t in "$gexc" "$cc" "$seed"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/lazy-units-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O2 -fPIC -shared -Wl,-soname,libb.so -o "$w/libb.so" "$here/libb.c" || exit 1
"$cc" -O2 -fPIC -shared -Wl,-soname,libd.so -o "$w/libd.so" "$here/libd.c" -L"$w" -lb || exit 1
"$cc" -O1 -pthread -o "$w/main" "$here/main.c" -L"$w" -lb -Wl,-rpath,"$w" -ldl || exit 1
want='main b_two 1007
main b_two rewritten 42
thread d_get 99
thread b_two 42
thread b_mul 42'
export GEX_CGEN_CACHE=$w/cgen GEX_ELF_UNITS=libb.so,libd.so GEX_SHARED=1
run() { # LOG [VAR=VALUE]
	(cd "$w" && env ${2:-} timeout "${TMO:-300}" node "$root/shims/browser/js/run-node.mjs" --cgen-prefer \
		--file "/lib/ld-musl-aarch64.so.1=$tc/aarch64-linux-musl/lib/libc.so" \
		--file "$w/libb.so=$w/libb.so" --file "$w/libd.so=$w/libd.so" --file "$w/main=$w/main" \
		"$seed" "$w/main" "$w" > "$1.out" 2> "$1.err")
}
# A first run finds where the loader maps the libraries; their units are built for those bases.
run "$w/probe"
for l in libb libd; do
	b=$(grep -ao "$l\.so at 0x[0-9a-f]*" "$w/probe.err" | head -1 | sed 's/.* at //')
	[ -n "$b" ] || { echo "FAIL lazy-units ($l.so never mapped)"; tail -20 "$w/probe.err"; exit 1; }
	GEX_CGEN_GUARD=1 "$gexc" cgen-image --elf "$w/$l.so" --base "$b" --jobs 2 > "$w/$l.cgen.log" 2>&1 || { cat "$w/$l.cgen.log"; exit 1; }
done
fail=0
for mode in deferred eager; do
	run "$w/$mode" "$([ $mode = eager ] && echo GEX_UNITS_EAGER=1)"; rc=$?
	placed=$(grep -ac 'lib[bd]\.so at 0x[0-9a-f]*: wanting its unit' "$w/$mode.err")
	got=$(grep -av '^\[' "$w/$mode.out")
	# and each found its unit (not left to tier-1)
	missing=$(grep -ac 'no function-tier unit' "$w/$mode.err")
	late=$(grep -ac 'tid [0-9]*: unit at [0-9]* instantiated at its first call' "$w/$mode.err")
	ok=1
	[ $rc = 0 ] && [ "$placed" = 2 ] && [ "$missing" = 0 ] && [ "$got" = "$want" ] || ok=0
	if [ $mode = deferred ]; then [ "$late" -ge 2 ] || ok=0; else [ "$late" = 0 ] || ok=0; fi
	if [ $ok = 1 ]; then
		echo "ok   lazy-units $mode (units placed $placed times; $late instantiated in the thread at a first call)"
	else
		echo "FAIL lazy-units $mode (rc=$rc$([ $rc = 124 ] && echo ': hung'), units placed $placed times, $missing without their unit, $late instantiated in the thread at a first call)"
		printf 'want:\n%s\ngot:\n%s\n' "$want" "$got"; grep -a 'trap\|rror' "$w/$mode.err" | head -5; fail=1
	fi
done
exit $fail
