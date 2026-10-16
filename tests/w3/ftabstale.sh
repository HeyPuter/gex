#!/bin/bash
# A thread that runs on across another thread's unit install (ftabstale.c,
# X's B16). Unit code calls a function pointer through the process-wide
# probe cache straight into its own Worker's function table, so the index
# another Worker published must be in this Worker's table. Before the fix a
# Worker's table grew only when it caught up at a turn out to JS, and a
# thread that last caught up before another's install of a large unit
# trapped ("table index is out of bounds"). The table's room is cut to 8
# here (GEX_TEST_FTAB_SPARE) so ftabdll's unit outruns it. In the page's
# runtime under Node (run-node.mjs), multi-Worker only: the single-thread
# build has one table, and the worker's spin would starve it.
#
#   tests/w3/ftabstale.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). The exe's unit is built at its fixed base; run 1 finds where
# ftabdll is placed, and its unit is built there (gexc cgen-image,
# precompiled, into this test's own cache). Needs i686-w64-mingw32-gcc, an
# x86 gexc, and the x86 shared-memory shim and seed in web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the units are built with it)"; exit 1; }
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip: no web/public/gex/$f"; exit 0; }
done
w=${WORK:-$root/target/w3-ftabstale}
rm -rf "$w"; mkdir -p "$w/units"
i686-w64-mingw32-gcc -O1 -o "$w/ftabstale.exe" "$here/ftabstale.c" || exit 1
i686-w64-mingw32-gcc -O1 -shared -nostdlib -e _DllMain@12 -o "$w/ftabdll.dll" "$here/ftabdll.c" -lkernel32 || exit 1
unit() { # IMAGE BASE
	(GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$gexc" cgen-image "$1" --pe-base "$2" --jobs 4 &&
		GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 GEX_CGEN_GUARD=1 "$gexc" cgen-image "$1" --pe-base "$2" --jobs 4) >> "$w/build.log" 2>&1 ||
		{ echo "FAIL: unit of $1 at $2 (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
}
page() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 GEX_CGEN_CACHE="$w/units" GEX_TEST_FTAB_SPARE=8 "$@" \
		timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/ftabstale.exe=$w/ftabstale.exe" --file "/t/ftabdll.dll=$w/ftabdll.dll" \
		--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/ftabstale.exe) > "$log" 2>&1
}
GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers > "$w/providers.log" 2>&1 ||
	{ echo "FAIL: provider modules (tail: $(tail -2 "$w/providers.log" | tr '\n' ' '))"; exit 1; }
unit "$w/ftabstale.exe" 0x400000
page "$w/p0.log"
base=$(sed -n 's/^ftabdll at \(0x\)\?\([0-9a-fA-F]*\).*/0x\2/p' "$w/p0.log" | head -1)
[ -n "$base" ] || { echo "FAIL: ftabdll not loaded"; grep -v '^\[host\]' "$w/p0.log" | tail -8 | sed 's/^/  /'; exit 1; }
unit "$w/ftabdll.dll" "$base"
page "$w/p1.log"
name="page MP, a thread calls into a unit installed while it ran"
for i in ftabstale.exe ftabdll.dll; do
	grep -q "loaded .*/$i (with its unit)" "$w/p1.log" || { echo "FAIL $name: $i ran without its unit"; grep "loaded .*/$i" "$w/p1.log" | head -2 | sed 's/^/  /'; exit 1; }
done
if grep -q "^ftabstale ok " "$w/p1.log"; then echo "ok   $name"; echo "1 passed, 0 failed"
else echo "FAIL $name"; grep -v '^\[host\]\|gex-console-error' "$w/p1.log" | tail -8 | sed 's/^/  /'; echo "0 passed, 1 failed"; exit 1; fi
