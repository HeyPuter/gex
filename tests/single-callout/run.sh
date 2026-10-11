#!/bin/bash
# Blocking inside a nested call-out (docs/single-thread.md §2, "Call-outs"):
# libcb_run is a function-tier unit (GEX_ELF_UNITS=libcb.so) and calls cb,
# which no unit holds and which blocks (a pipe read, nanosleep). In the
# single-thread build call_out / ftab_call are Suspending imports that run
# the nested guest on its own promising stack, so the block parks only this
# fiber; before that it trapped with "trying to suspend JS frames". Runs the
# program under run-node.mjs in single mode (SharedArrayBuffer deleted) and
# in the SAB build; both must place the unit and print the right sum.
#
#   tests/single-callout/run.sh [gexc]
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN) and the shared
# browser build in web/public/gex (shims/browser/build.sh with GEX_SHARED=1).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/release/gexc}")
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
seed=$root/web/public/gex/seed.shared.wasm
for t in "$gexc" "$cc" "$seed"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/single-callout-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O2 -fPIC -shared -Wl,-soname,libcb.so -o "$w/libcb.so" "$here/lib.c" || exit 1
"$cc" -O1 -o "$w/main" "$here/main.c" -L"$w" -lcb -Wl,-rpath,"$w" || exit 1
export GEX_CGEN_CACHE=$w/cgen GEX_ELF_UNITS=libcb.so
# The single build needs JSPI: on by default from node 25, behind a flag before.
jspi=
node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null ||
	jspi=--experimental-wasm-jspi
run() { # MODE-VAR LOG
	(cd "$w" && env "$1=1" timeout "${TMO:-300}" node $jspi --import 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer' \
		"$root/shims/browser/js/run-node.mjs" --cgen-prefer \
		--file "/lib/ld-musl-aarch64.so.1=$tc/aarch64-linux-musl/lib/libc.so" \
		--file "$w/libcb.so=$w/libcb.so" --file "$w/main=$w/main" "$seed" "$w/main" > "$2.out" 2> "$2.err")
}
# A first run finds where the loader maps libcb.so; its unit is built for that base.
run GEX_SINGLE "$w/probe"
base=$(grep -ao 'libcb\.so at 0x[0-9a-f]*' "$w/probe.err" | head -1 | sed 's/.* at //')
[ -n "$base" ] || { echo "FAIL single-callout (libcb.so never mapped)"; tail -20 "$w/probe.err"; exit 1; }
GEX_CGEN_GUARD=1 "$gexc" cgen-image --elf "$w/libcb.so" --base "$base" --jobs 2 > "$w/cgen.log" 2>&1 || { cat "$w/cgen.log"; exit 1; }
fail=0
for m in GEX_SINGLE GEX_SHARED; do
	run $m "$w/$m"; rc=$?
	placed=$(grep -ac 'function tier for .*libcb\.so' "$w/$m.err")
	if [ $rc = 0 ] && [ "$placed" -ge 1 ] && grep -q '^callback read w$' "$w/$m.out" && grep -q '^libcb_run = 100 ok$' "$w/$m.out"; then
		echo "ok   single-callout $m"
	else
		echo "FAIL single-callout $m (rc=$rc, unit placed $placed times)"; cat "$w/$m.out"; grep -a 'trap\|rror' "$w/$m.err" | head -5; fail=1
	fi
done
exit $fail
