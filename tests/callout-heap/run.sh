#!/bin/bash
# A call-out's loop keeps nothing per trap (gex.mjs `#startThread`): libcb_run
# is a function-tier unit (GEX_ELF_UNITS=libcb.so, tests/single-callout's
# lib.c) calling cb, which no unit holds, N times, and cb raises a signal, a
# trap inside the call-out. A Wine thread's whole life can be one call-out, so
# such a loop never reaches the event loop: a promise made per trap (and its
# handler, holding the trap's error) waited in the microtask queue for good.
# Hades x64's page queued 4 GB of JS heap that way and the renderer died
# ("V8 javascript OOM"). Under run-node.mjs (shared build), stretch.mjs counts
# the most promises a thread made with no async callback between: it must
# not grow with N.
#
#   tests/callout-heap/run.sh [gexc]
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN) and the shared
# browser build in web/public/gex (shims/browser/build.sh with GEX_SHARED=1).
set -u
export GEX_LOG=${GEX_LOG:-1}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/release/gexc}")
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
seed=$root/web/public/gex/seed.shared.wasm
for t in "$gexc" "$cc" "$seed"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/callout-heap-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O2 -fPIC -shared -Wl,-soname,libcb.so -o "$w/libcb.so" "$here/../single-callout/lib.c" || exit 1
"$cc" -O1 -o "$w/main" "$here/main.c" -L"$w" -lcb -Wl,-rpath,"$w" || exit 1
export GEX_CGEN_CACHE=$w/cgen GEX_ELF_UNITS=libcb.so
run() { # LOG N
	(cd "$w" && GEX_SHARED=1 timeout "${TMO:-300}" node --import "$here/stretch.mjs" \
		"$root/shims/browser/js/run-node.mjs" --cgen-prefer \
		--file "/lib/ld-musl-aarch64.so.1=$tc/aarch64-linux-musl/lib/libc.so" \
		--file "$w/libcb.so=$w/libcb.so" --file "$w/main=$w/main" "$seed" "$w/main" "$2" > "$1.out" 2> "$1.err")
}
# A first run finds where the loader maps libcb.so; its unit is built for that base.
run "$w/probe" 2
base=$(grep -ao 'libcb\.so at 0x[0-9a-f]*' "$w/probe.err" | head -1 | sed 's/.* at //')
[ -n "$base" ] || { echo "FAIL callout-heap (libcb.so never mapped)"; tail -20 "$w/probe.err"; exit 1; }
GEX_CGEN_GUARD=1 "$gexc" cgen-image --elf "$w/libcb.so" --base "$base" --jobs 2 > "$w/cgen.log" 2>&1 || { cat "$w/cgen.log"; exit 1; }
most() { grep -ao '\[stretch\] thread [0-9]*: [0-9]* promises' "$1" | awk '{if ($4 > m) m = $4} END {print m + 0}'; }
fail=0
for n in 100 3000; do
	run "$w/n$n" $n; rc=$?
	placed=$(grep -ac 'function tier for .*libcb\.so' "$w/n$n.err")
	eval "most_$n=$(most "$w/n$n.err")"
	if [ $rc != 0 ] || [ "$placed" -lt 1 ] || ! grep -q ' ok$' "$w/n$n.out"; then
		echo "FAIL callout-heap n=$n (rc=$rc, unit placed $placed times)"; cat "$w/n$n.out"; grep -a 'trap\|rror' "$w/n$n.err" | head -5; fail=1
	fi
done
# Before the fix: 256 for 100 call-outs, 8192 for 3000 (about 2.7 a trap).
if [ $fail = 0 ] && [ "$most_3000" -le 64 ] && [ "$most_3000" -le $((most_100 * 2 + 16)) ]; then
	echo "ok   callout-heap (most promises in one stretch: $most_100 for 100 call-outs, $most_3000 for 3000)"
else
	[ $fail = 0 ] && echo "FAIL callout-heap: $most_100 promises in one stretch for 100 call-outs, $most_3000 for 3000"
	fail=1
fi
exit $fail
