#!/bin/bash
# A call-out whose return is an entry of the unit that made it: co_call
# (lib.S, a function-tier unit) calls cb, which no unit holds, and cb returns
# to co_after's entry, which dispatch reaches through the unit's bridge, so
# the call-out sees its return through the watched entry (gex.mjs
# `#watchHit`). The watch compares the guest's stack pointer with the one the
# call-out waits for, read from the thread's context at the architecture's
# place: read from anywhere else, no return was ever the call-out's, each
# call-out ran on inside the last and they nested until the depth limit.
# Every call-out must end at its own return (depth 0 throughout).
#
#   tests/callout-entry-return/run.sh [gexc]
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN) and the shared
# browser build in web/public/gex (shims/browser/build.sh).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/release/gexc}")
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
seed=$root/web/public/gex/seed.shared.wasm
for t in "$gexc" "$cc" "$seed"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/callout-entry-return-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O2 -fPIC -shared -Wl,-soname,libco.so -o "$w/libco.so" "$here/lib.S" || exit 1
"$cc" -O1 -o "$w/main" "$here/main.c" -L"$w" -lco -Wl,-rpath,"$w" || exit 1
export GEX_CGEN_CACHE=$w/cgen GEX_ELF_UNITS=libco.so
run() { # LOG N
	(cd "$w" && GEX_SHARED=1 timeout "${TMO:-300}" node "$root/shims/browser/js/run-node.mjs" --cgen-prefer \
		--file "/lib/ld-musl-aarch64.so.1=$tc/aarch64-linux-musl/lib/libc.so" \
		--file "$w/libco.so=$w/libco.so" --file "$w/main=$w/main" "$seed" "$w/main" "$2" > "$1.out" 2> "$1.err")
}
# A first run finds where the loader maps libco.so; its unit is built for that base.
run "$w/probe" 2
base=$(grep -ao 'libco\.so at 0x[0-9a-f]*' "$w/probe.err" | head -1 | sed 's/.* at //')
[ -n "$base" ] || { echo "FAIL callout-entry-return (libco.so never mapped)"; tail -20 "$w/probe.err"; exit 1; }
GEX_CGEN_GUARD=1 "$gexc" cgen-image --elf "$w/libco.so" --base "$base" --jobs 2 > "$w/cgen.log" 2>&1 || { cat "$w/cgen.log"; exit 1; }
n=40
GEX_CALLOUT_TRACE=1 run "$w/run" $n; rc=$?
made=$(grep -ac '\[callout\] #' "$w/run.err")
deep=$(grep -ao '\[callout\] #[0-9]* depth [0-9]*' "$w/run.err" | awk '$4 > 0' | wc -l)
if [ $rc = 0 ] && grep -q '^ok$' "$w/run.out" && [ "$made" -ge $n ] && [ "$deep" = 0 ]; then
	echo "ok   callout-entry-return ($made call-outs, each ended at its return)"
else
	echo "FAIL callout-entry-return (rc=$rc, $made call-outs, $deep made inside another)"; cat "$w/run.out"
	grep -a '\[callout\]' "$w/run.err" | head -8
	exit 1
fi
