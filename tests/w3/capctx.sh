#!/bin/bash
# RtlCaptureContext on W3 (capctx.c): i386's fields only, nothing at or past
# offset 0xcc, Ebp/Eip/Esp from the caller's frame, and a capture resumed
# with NtContinue returning from its function, as Wine Mono's
# release_call_wrapper does (Antichamber). Prints "capctx ok".
# Natively (gexc run) and in the page's runtime under Node (run-node.mjs).
#
#   tests/w3/capctx.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). Needs i686-w64-mingw32-gcc, the x86 shared-memory shim and seed
# in web/public/gex and, for the native legs, an x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
w=${WORK:-$root/target/w3-capctx}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -fno-omit-frame-pointer -o "$w/capctx.exe" "$here/capctx.c" -lntdll || exit 1
want="capctx ok"
pass=0 fail=0
verdict() { # NAME LOG
	if grep -q "^$want" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -8 | sed 's/^/  /'; fi
}
for mode in fast; do
	extra=()
	if [ -x "$gexc" ]; then
		rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/capctx.exe" "$w/pfx-native/drive_c/"
		(cd "$w/pfx-native/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run \
			--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all "${extra[@]}" \
			"$dist/lib/wine/i386-unix/wine" 'C:\capctx.exe') > "$w/native-$mode.log" 2>&1
		verdict "native $mode" "$w/native-$mode.log"
	else
		echo "skip native: no $gexc"
	fi
	for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
		[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; continue 2; }
	done
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/capctx.exe=$w/capctx.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all "${extra[@]}" \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/capctx.exe) > "$w/page-$mode.log" 2>&1
	verdict "page $mode" "$w/page-$mode.log"
done
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
