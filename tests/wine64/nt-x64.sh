#!/bin/bash
# W64.1 (docs/wine-native-x64.md §5): tests/nt's x64 programs on 64-bit
# native Wine, its Unix side wasm64 lowered to wasm32 (link-w64.sh), its
# x86_64 PE DLLs translated, the W3 wineserver serving a win64 prefix, no
# doors (WINEGEX_DOOR=0). Each case checks stdout and the exit code, as
# tests/nt/run.sh does, and highdata.c (built with x86_64-w64-mingw32-gcc).
#
#   tests/wine64/nt-x64.sh DIST PREFIX [gexc]
#
# DIST    W64 install: lib/wine/x86_64-unix/{wine,wineserver} (the loader
#         link-w64.sh links, the wasm32 server link-wineserver.sh links from
#         the same Wine tree), lib/wine/x86_64-windows (the fork's PE DLLs,
#         MinGW), share/wine
# PREFIX  a win64 prefix made from the same Wine (copied, never written)
# gexc    an x86 gexc (default target/x86/release/gexc)
# CASES="hello_args sync" limits the cases; TMO the per-case timeout (s).
# W64_TOOLS (w64-dist.sh's: a native build of the fork's tools) and W64_SRC
# (the Wine tree) add the native PE side's entry-adapter case, `dll`
# (tests/wasm64/dll, W64.2; needs GEX_WASM64_SYSROOT).
# NTDLL=FILE runs the cases with that ntdll.dll in place of DIST's (W64.2's
# native PE ntdll, w64-dist.sh's W64_NTDLL; the other DLLs translated).
# NATIVE=DIR runs them with every file in DIR in place of (or beside) DIST's
# PE DLLs: W64.2's skeletons over one module, winepe.wasm (w64-dist.sh's
# W64_NATIVE, pelink.py).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "no gexc at $gexc" >&2; exit 2; }
wine=$dist/lib/wine/x86_64-unix/wine
[ -f "$wine" ] && [ -f "$dist/lib/wine/x86_64-unix/wineserver" ] || { echo "no W64 loader and server in $dist" >&2; exit 2; }
bin=$root/tests/nt/bin/x64
w=${WORK:-$(mktemp -d)}
mkdir -p "$w"
tag=W64
if [ -n "${NTDLL:-}${NATIVE:-}" ]; then
	# DIST with those DLLs: the Unix side copied (the loader finds the PE
	# DLLs beside itself), links to the other DLLs
	[ -z "${NTDLL:-}" ] || [ -f "$NTDLL" ] || { echo "no ntdll at $NTDLL" >&2; exit 2; }
	[ -z "${NATIVE:-}" ] || [ -f "$NATIVE/winepe.wasm" ] || { echo "no winepe.wasm in $NATIVE" >&2; exit 2; }
	o=$w/dist; rm -rf "$o"; mkdir -p "$o/lib/wine/x86_64-windows"
	cp -a "$dist/lib/wine/x86_64-unix" "$o/lib/wine/"
	ln -s "$dist/share" "$o/share"; ln -s ../../share "$o/lib/wine/share"
	for f in "$dist"/lib/wine/x86_64-windows/*; do ln -s "$f" "$o/lib/wine/x86_64-windows/"; done
	if [ -n "${NTDLL:-}" ]; then
		rm "$o/lib/wine/x86_64-windows/ntdll.dll"; cp "$NTDLL" "$o/lib/wine/x86_64-windows/ntdll.dll"
		tag="W64 native ntdll"
	fi
	if [ -n "${NATIVE:-}" ]; then
		for f in "$NATIVE"/*; do
			rm -f "$o/lib/wine/x86_64-windows/${f##*/}"; cp "$f" "$o/lib/wine/x86_64-windows/"
		done
		tag="W64 native $(cd "$NATIVE" && ls -- *.dll *.exe 2>/dev/null | sed 's/\.[^.]*$//' | tr '\n' ' ' | sed 's/ $//')"
	fi
	dist=$o; wine=$dist/lib/wine/x86_64-unix/wine
fi
nl=$'\n'
allok="(ok [^$nl]*$nl)+0 failures"
pass=0 fail=0
# check NAME EXPECTED_EXIT EXPECTED_STDOUT(regex, whole output) EXE [ARGS...]
check() {
	local name=$1 want_code=$2 want_out=$3 exe=$4; shift 4
	case " ${CASES:-$name} " in *" $name "*) ;; *) return ;; esac
	local p=$w/pfx-$name out code src=$bin/$exe
	case $exe in /*) src=$exe; exe=$(basename "$exe") ;; esac
	rm -rf "$p"; cp -a "$prefix" "$p"; cp "$src" ${EXTRA:-} "$p/drive_c/"
	out=$(cd "$p/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$p" --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=0 \
		"$wine" "C:\\$exe" "$@" 2>"$w/$name.err"; echo "exit=$?")
	code=${out##*exit=}
	out=${out%$'\n'exit=*}; out=${out%exit=*}; out=${out//$'\r'/}
	if [ "$code" = "$want_code" ] && [[ "$out" =~ ^${want_out}$ ]]; then
		pass=$((pass + 1)); echo "ok   x64/$name ($tag)"
	else
		fail=$((fail + 1)); echo "FAIL x64/$name ($tag): exit $code (want $want_code)"
		echo "  stdout: $(printf %q "$out" | cut -c1-400)"
		grep -v '^\[host\]' "$w/$name.err" | tail -8 | sed 's/^/  stderr: /'
	fi
	rm -rf "$p"
}
check hello_args 42 "hello from a Windows program with 3 args${nl}arg 1: a${nl}arg 2: b c" hello_args.exe a "b c"
check sync 0 "$allok" sync.exe
check mtx 0 "$allok" mtx.exe
check crtmem 0 "$allok" crtmem.exe
check apc 0 "$allok" apc.exe
check tls 0 "$allok" tls.exe
check cxx 0 "$allok" cxx.exe
check exitproc 33 "worker exiting${nl}atexit ran" exitproc.exe
check seh 77 "(ok [^$nl]*$nl)+top-level filter saw e00000ff${nl}0 failures" seh.exe
check seh-traps 0 "$allok" seh.exe traps
# W64.2: MSVC's x64 C++ exceptions, SEH and setjmp/longjmp through msvcrt
# (tests/nt/msvc/build-x64.sh)
check msvc_cxxeh 0 "$allok" msvc_cxxeh.exe
check fiber 0 "$allok" fiber.exe
# W64.1b: data above 2 GiB, code below it (highdata.c)
if x86_64-w64-mingw32-gcc -O1 -o "$w/highdata.exe" "$here/highdata.c"; then
	check highdata 0 "$allok" "$w/highdata.exe"
	# the refusal says why, in Wine's log
	case " ${CASES:-highdata} " in *" highdata "*)
		grep -q "made executable above gex's code space" "$w/highdata.err" ||
			{ fail=$((fail + 1)); echo "FAIL x64/highdata ($tag): no log line for the refused protection"; } ;;
	esac
else
	fail=$((fail + 1)); echo "FAIL x64/highdata (W64): x86_64-w64-mingw32-gcc"
fi
# W64.2: a native DLL (a lowered wasm64 module behind a PE32+ skeleton)
# called from x64 code through every kind of Win64 entry adapter, and
# calling back (tests/wasm64/dll)
if [ -n "${W64_TOOLS:-}" ] && [ -n "${W64_SRC:-}" ]; then
	case " ${CASES:-dll} " in *" dll "*)
		if "$root/tests/wasm64/dll/build.sh" "$w/dll" "$W64_SRC" "$W64_TOOLS" > "$w/dll.build" 2>&1; then
			EXTRA="$w/dll/abi.dll" check dll 0 "(ok [^$nl]*$nl)+PASS" "$w/dll/abi_test.exe"
		else
			fail=$((fail + 1)); echo "FAIL x64/dll (W64): build"; tail -5 "$w/dll.build" | sed 's/^/  /'
		fi ;;
	esac
fi
[ -n "${WORK:-}" ] || rm -rf "$w"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
