#!/bin/bash
# Bridged dispatch slots share entries in `blocks` (gex_emit::dense::
# bridge_words: one entry per 256 bridged slots, each slot an arm of it),
# and the bridge takes its target from the pc the caller dispatched on,
# never from the entry or arm (main.c). Two unit entries called alternately
# from one call site must each run their own code, and a unit's code
# unmapped (its pages retired, their slots cleared) must leave the other
# unit's bridged slots, which share its entry, working. Runs under run-node.mjs (the SAB build, aarch64), with
# GEX_LOG=bridges counting the entries written and the addresses entered.
#
#   tests/bridge-share/run.sh [gexc]
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN), gcc (the
# expected output), and the shared browser build in web/public/gex
# (shims/browser/build.sh with GEX_SHARED=1).
set -u
# The [bridges] lines are debug output.
export GEX_LOG=${GEX_LOG:-bridges}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/release/gexc}")
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
seed=$root/web/public/gex/seed.shared.wasm
for t in "$gexc" "$cc" "$seed"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/bridge-share-test}
rm -rf "$w"; mkdir -p "$w/host"
for d in "$w:$cc" "$w/host:gcc"; do
	o=${d%%:*}; c=${d#*:}
	"$c" -O2 -fPIC -shared -DK=3 -DC=1 -Wl,-soname,liba.so -o "$o/liba.so" "$here/lib.c" || exit 1
	"$c" -O2 -fPIC -shared -DK=7 -DC=-2 -Wl,-soname,libb.so -o "$o/libb.so" "$here/lib.c" || exit 1
	"$c" -O1 -o "$o/main" "$here/main.c" -L"$o" -lb -Wl,-rpath,"$o" -ldl || exit 1
done
want=$("$w/host/main" "$w/host")
export GEX_CGEN_CACHE=$w/cgen GEX_ELF_UNITS=liba.so,libb.so GEX_SHARED=1
run() { # LOG
	(cd "$w" && timeout "${TMO:-300}" node "$root/shims/browser/js/run-node.mjs" --cgen-prefer \
		--file "/lib/ld-musl-aarch64.so.1=$tc/aarch64-linux-musl/lib/libc.so" \
		--file "$w/liba.so=$w/liba.so" --file "$w/libb.so=$w/libb.so" --file "$w/main=$w/main" \
		"$seed" "$w/main" "$w" > "$1.out" 2> "$1.err")
}
# A first run finds where the loader maps the libraries; their units are built for those bases.
run "$w/probe"
for l in liba libb; do
	b=$(grep -ao "$l\.so at 0x[0-9a-f]*" "$w/probe.err" | head -1 | sed 's/.* at //')
	[ -n "$b" ] || { echo "FAIL bridge-share ($l.so never mapped)"; tail -20 "$w/probe.err"; exit 1; }
	GEX_CGEN_GUARD=1 "$gexc" cgen-image --elf "$w/$l.so" --base "$b" --jobs 2 > "$w/$l.cgen.log" 2>&1 || { cat "$w/$l.cgen.log"; exit 1; }
done
run "$w/run"; rc=$?
placed=$(grep -ac 'lib[ab]\.so at 0x[0-9a-f]*: wanting its unit' "$w/run.err")
got=$(grep -av '^\[' "$w/run.out")
# The main thread's last line: entries written, addresses entered.
last=$(grep -a '\[bridges\] pid 1 main thread' "$w/run.err" | tail -1)
entered=$(sed -n 's/.*entered \([0-9]*\) addresses.*/\1/p' <<< "$last")
written=$(sed -n 's/.*, \([0-9]*\) bridge entries.*/\1/p' <<< "$last")
if [ $rc = 0 ] && [ "$placed" = 2 ] && [ "$got" = "$want" ] && [ "${entered:-0}" -ge 6 ]; then
	echo "ok   bridge-share (units placed $placed times; $entered addresses entered through $written bridge entry writes; output as native)"
else
	echo "FAIL bridge-share (rc=$rc$([ $rc = 124 ] && echo ': hung'), units placed $placed times, ${entered:-no} addresses entered)"
	printf 'want:\n%s\ngot:\n%s\n' "$want" "$got"; grep -a 'trap\|rror' "$w/run.err" | head -5; exit 1
fi

# Units off, a native wasm32 program: its module goes in through the function
# table's log all the same. An exec from a second thread keeps that thread's
# Worker, and its `blocks` already holds the entry the new program's bridges
# take arms of, so dispatch reaches the bridge (and `gex.lookup`) before any
# miss: lookup must catch the Worker up on the log, or the slot resolves to
# `resolve` for ever (cgen.rs `behind`). The hang was intermittent, so a few
# runs. Needs GEX_WASM_SYSROOT (tools/wine-wasm/build-sysroot.sh).
if [ -z "${GEX_WASM_SYSROOT:-}" ]; then
	echo "skip bridge-share exec (GEX_WASM_SYSROOT unset)"; exit 0
fi
"$root/tools/wine-wasm/wcc" -O2 -o "$w/execthreads" "$root/tests/wasm32/execthreads.c" || exit 1
for i in 1 2 3; do
	got=$(cd "$w" && env -u GEX_CGEN_CACHE -u GEX_ELF_UNITS GEX_LOG=off timeout "${TMO:-120}" node "$root/shims/browser/js/run-node.mjs" \
		--file /t/execthreads="$w/execthreads" --cwd /t --argv0 ./execthreads "$seed" "$w/execthreads" 2> "$w/exec$i.err" < /dev/null)
	rc=$?
	if [ $rc != 0 ] || ! grep -q 'exec from a second thread: 4 of 4' <<< "$got"; then
		echo "FAIL bridge-share exec (run $i, rc=$rc$([ $rc = 124 ] && echo ': hung'))"; printf '%s\n' "$got"; exit 1
	fi
done
echo "ok   bridge-share exec (exec from a second thread with units off, 3 runs)"
