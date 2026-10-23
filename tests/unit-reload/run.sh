#!/bin/bash
# A unit's library unloaded and another loaded at its base while a second
# thread sleeps, then that thread calls in through another unit (main.c), in
# the browser runtime (shims/browser/js/run-node.mjs, x86, the shared build).
# Before cgen::on_retire cleared a retired unit's probe cache entries, the
# thread's Worker -- its table still holding the old unit's body -- entered
# that body from the cache, and the body, its page patched, left through the
# same entry to itself: the run hung (Windows 7's uxtheme.dll reloaded in
# explorer /desktop, its DllMain spinning with the loader lock held).
#
#   tests/unit-reload/run.sh [gexc]   (gcc and the host's glibc as the guest's)
#
# Needs `GEX_ARCH=x86 GEX_SHARED=1 shims/browser/build.sh`.
set -u
# The lines checked below ("function tier for", "replaced by") are debug output.
export GEX_LOG=${GEX_LOG:-1}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/x86/release/gexc}")
for t in "$gexc" "$root/web/public/gex/gex_browser.x86.shared.wasm"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/unit-reload-test}
rm -rf "$w"; mkdir -p "$w"
gcc -O1 -shared -fPIC -DK=3 -DC=1 -o "$w/liba.so" "$here/lib.c" || exit 1
gcc -O1 -shared -fPIC -DK=7 -DC=-2 -o "$w/libb.so" "$here/lib.c" || exit 1
# A real call through the pointer (not a tail jump), so it returns into the unit.
gcc -O1 -fno-optimize-sibling-calls -shared -fPIC -Wl,-soname,libcall.so -o "$w/libcall.so" "$here/call.c" || exit 1
gcc -O1 -pthread -o "$w/main" "$here/main.c" -L"$w" -lcall -Wl,-rpath,"$w" -ldl || exit 1
"$gexc" build "$w/main" -o "$w/main.wasm" > "$w/build.log" 2>&1 || { tail -5 "$w/build.log"; exit 1; }
native=$("$w/main" "$w" 2> /dev/null)
libs=""
for f in /lib64/ld-linux-x86-64.so.2 /lib/x86_64-linux-gnu/libc.so.6; do libs="$libs --file $f=$(realpath "$f")"; done
export GEX_CGEN_CACHE=$w/cgen GEX_ELF_UNITS=lib.so,libcall.so GEX_ARCH=x86 GEX_SHARED=1
run() { # LOG
	# shellcheck disable=SC2086
	(cd "$w" && timeout "${TMO:-120}" node "$root/shims/browser/js/run-node.mjs" --cgen-prefer $libs \
		--file "$w/main=$w/main" --file "$w/liba.so=$w/liba.so" --file "$w/libb.so=$w/libb.so" --file "$w/libcall.so=$w/libcall.so" \
		"$w/main.wasm" "$w/main" "$w" > "$1.out" 2> "$1.err")
}
# A first run (no units yet) finds where the loader maps the two libraries.
run "$w/probe"
ba=$(grep -ao '/lib\.so at 0x[0-9a-f]*' "$w/probe.err" | head -1 | sed 's/.* at //')
bc=$(grep -ao 'libcall\.so at 0x[0-9a-f]*' "$w/probe.err" | head -1 | sed 's/.* at //')
[ -n "$ba" ] && [ -n "$bc" ] || { echo "FAIL unit-reload (no base for liba.so '$ba' or libcall.so '$bc')"; tail -20 "$w/probe.err"; exit 1; }
for l in liba:$ba libcall:$bc; do
	GEX_CGEN_GUARD=1 "$gexc" cgen-image --elf "$w/${l%%:*}.so" --base "${l#*:}" --jobs 2 > "$w/${l%%:*}.cgen.log" 2>&1 || { cat "$w/${l%%:*}.cgen.log"; exit 1; }
done
run "$w/run"; rc=$?
placed=$(grep -ac 'function tier for .*/lib\(call\)\?\.so' "$w/run.err")
gex=$(grep -av '^\[' "$w/run.out")
if [ $rc = 0 ] && [ "$placed" -ge 2 ] && [ "$gex" = "$native" ] && grep -q 'same-base, f same-address' "$w/run.out"; then
	echo "ok   unit-reload (units placed $placed times; output as native)"
else
	echo "FAIL unit-reload (rc=$rc$([ $rc = 124 ] && echo ': hung'), units placed $placed times)"
	printf 'native:\n%s\ngex:\n%s\n' "$native" "$gex"; grep -a 'trap\|rror' "$w/run.err" | head -5; exit 1
fi
