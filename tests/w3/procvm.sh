#!/bin/bash
# One process reaching another's memory, natively (gexc run):
#   vmpeer   ReadProcessMemory/WriteProcessMemory on a child (reads, writes,
#            a write over its code, a partial copy across a decommitted
#            page, a read-only page, an exited child);
#   dbgflag  a debuggee's PEB BeingDebugged, both when it is created under a
#            debugger and when one attaches (the server writes it with
#            process_vm_writev);
#   winedbg  the system debugger attaching to a process whose exception
#            nothing handles;
#   remotestart  threads started in another process: a native function's
#            value refused, a guest address run, a debugger's break-ins
#            (whose start is ntdll's export) reaching it, and a fault
#            the debugger sees first (first chance) and passes on.
# The page has no process_vm_readv/writev (ENOSYS): there only vmpeer, whose
# copies the target process makes itself.
# A write over code compiled into a unit is not seen by that unit, so vmpeer's
# code patch is checked on translated code only (no units here).
# "vmpeer quick" makes its first access while the child is still starting
# (no settling sleep), when the target's copy, run in its SIGUSR1 handler,
# may interrupt the child's own malloc: five runs natively, one in the page.
#
#   tests/w3/procvm.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). WORK (default target/w3-procvm) must be on disk.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc"; exit 1; }
w=${WORK:-$root/target/w3-procvm}
rm -rf "$w"; mkdir -p "$w"
for p in vmpeer dbgflag remotestart; do i686-w64-mingw32-gcc -O1 -o "$w/$p.exe" "$here/$p.c" || exit 1; done
pass=0 fail=0
ok() { pass=$((pass + 1)); echo "ok   $1"; }
bad() { fail=$((fail + 1)); echo "FAIL $1"; shift; for l in "$@"; do echo "  $l"; done; }
native() { # LOG TIMEOUT EXE [ARGS]
	local log=$1 t=$2; shift 2
	rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"; cp "$w"/*.exe "$w/pfx/drive_c/"
	# The debugger's crash dialog waits for a click: its text report instead.
	printf '\n[Software\\\\Wine\\\\WineDbg] 1700000000\n"ShowCrashDialog"=dword:00000000\n' >> "$w/pfx/user.reg"
	(cd "$w/pfx/drive_c" && timeout -k 10 "$t" "$gexc" run --env WINEPREFIX="$w/pfx" \
		--env WINEDEBUG=err+all,fixme-all "$dist/lib/wine/i386-unix/wine" "$@") 2>&1 | tr -d '\r' > "$log"
}
vmpeer() { # NAME LOG
	if grep -q '^vmpeer done, 0 failed' "$2" && grep -q '^child exit 55' "$2"; then ok "$1"
	elif ! grep -q '^FAIL\|^vmpeer done' "$2"; then bad "$1" "hung after '$(grep '^ok' "$2" | tail -1 | cut -c6-)'"
	else bad "$1" "$(grep -E '^(FAIL|child exit)' "$2" | head -4 | tr '\n' ';')"; fi
}
native "$w/vmpeer.log" 150 'C:\vmpeer.exe'
vmpeer "native, vmpeer" "$w/vmpeer.log"
for i in 1 2 3 4 5; do
	native "$w/quick-$i.log" 90 'C:\vmpeer.exe' quick
	vmpeer "native, vmpeer quick $i" "$w/quick-$i.log"
done
native "$w/dbgflag.log" 300 'C:\dbgflag.exe'
if grep -q '^dbgflag done, 0 failed' "$w/dbgflag.log"; then ok "native, dbgflag"
else bad "native, dbgflag" "$(grep -E '^FAIL|Assertion' "$w/dbgflag.log" | head -3 | tr '\n' ';')"; fi
native "$w/winedbg.log" 180 'C:\vmpeer.exe' crash
if grep -q '^WineDbg attached to pid' "$w/winedbg.log" && ! grep -q "Can't attach\|gex failed" "$w/winedbg.log"; then ok "native, winedbg attaches"
else bad "native, winedbg attaches" "$(grep -E "attach|starting debugger|gex failed" "$w/winedbg.log" | head -3 | tr '\n' ';')"; fi
native "$w/remotestart.log" 180 'C:\remotestart.exe'
if grep -q '^remotestart done, 0 failed' "$w/remotestart.log"; then ok "native, remotestart"
else bad "native, remotestart" "$(grep -E '^FAIL|gex failed' "$w/remotestart.log" | head -4 | tr '\n' ';')"; fi
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 200 node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
	--file "/t/vmpeer.exe=$w/vmpeer.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" /t/vmpeer.exe) 2>&1 | tr -d '\r' > "$w/page.log"
vmpeer "page, vmpeer" "$w/page.log"
rm -rf "$w/pfx-page"; cp -a "$prefix" "$w/pfx-page"
(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 200 node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
	--file "/t/vmpeer.exe=$w/vmpeer.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" /t/vmpeer.exe quick) 2>&1 | tr -d '\r' > "$w/page-quick.log"
vmpeer "page, vmpeer quick" "$w/page-quick.log"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
