#!/bin/bash
# W64.1 (docs/wine-native-x64.md §5): tests/nt's x64 programs on 64-bit
# native Wine, its Unix side wasm64 lowered to wasm32 (link-w64.sh), its
# x86_64 PE DLLs translated, the W3 wineserver serving a win64 prefix, no
# doors (WINEGEX_DOOR=0; DOOR=1 runs with them, W64.3's LP64 door call,
# which needs a loader with `gex.door_call_lp64`). Each case checks stdout
# and the exit code, as
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
# The audio case: waveOut and mmdevapi through wineoss.drv (audio.c).
# The authattrs case: crypt32 verifies a signature over authenticated
# attributes as encoded, not in DER order (authattrs.c).
# The display case (W64.4), when the loader has winewayland.drv's Unix side
# (link-w64.sh with W64_DEPS) and the driver's PE side is there (native:
# NATIVE= with W64_NATIVE_DLLS' winewayland.drv): window.exe under gexc's
# Wayland compositor, its frames checked for the four quadrants and the
# white square (frame.py).
# GL=1 runs a case with the host's GL (gexc run --gl): the GL cases (W64.4),
# when the loader has opengl32.so and gex-gl's EGL.
# NATIVE=DIR runs them with every file in DIR in place of (or beside) DIST's
# PE DLLs: W64.2's skeletons over one module, winepe.wasm (w64-dist.sh's
# W64_NATIVE, pelink.py).
# PAGE=mp|sp (W64.5) runs the cases in the browser runtime instead
# (shims/browser/js/run-node.mjs, the shim and glue a page loads, under
# Node): mp the default build (SharedArrayBuffer), sp the single-thread one
# (GEX_SINGLE=1, no SharedArrayBuffer, JSPI). It needs the x86 shared shim
# and seed in web/public/gex (GEX_ARCH=x86 shims/browser/build.sh), and
# runs on a page prefix template made from PREFIX (page-prefix.sh). With
# NATIVE or NTDLL the overlay is hard links (a page mount does not follow
# a link out of it). Not with WL or GL.
set -u
# Output is compared with the native run: not even gex's default lines (log.mjs).
export GEX_LOG=${GEX_LOG:-off}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -n "${PAGE:-}" ] || [ -x "$gexc" ] || { echo "no gexc at $gexc" >&2; exit 2; }
wine=$dist/lib/wine/x86_64-unix/wine
[ -f "$wine" ] && [ -f "$dist/lib/wine/x86_64-unix/wineserver" ] || { echo "no W64 loader and server in $dist" >&2; exit 2; }
bin=$root/tests/nt/bin/x64
door=0; [ "${DOOR:-0}" = 1 ] && door=1
single=; [ "${PAGE:-}" = sp ] && single=1
w=${WORK:-$(mktemp -d)}
mkdir -p "$w"
tag=W64
page=${PAGE:-}
if [ -n "$page" ]; then
	case $page in mp|sp) ;; *) echo "PAGE is mp or sp" >&2; exit 2 ;; esac
	[ -z "${GL:-}" ] || { echo "PAGE runs without GL" >&2; exit 2; }
	seed=$root/web/public/gex/seed.x86.shared.wasm
	[ -f "$seed" ] && [ -f "$root/web/public/gex/gex_browser.x86.shared.wasm" ] ||
		{ echo "no x86 shared shim and seed in web/public/gex; GEX_ARCH=x86 shims/browser/build.sh" >&2; exit 2; }
	# the single-thread build needs JSPI (on by default from node 25)
	jspi=; node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null || jspi=--experimental-wasm-jspi
	"$root/tools/wine-wasm/page-prefix.sh" "$prefix" "$dist" "$w/page-pfx" > "$w/page-pfx.log" 2>&1 ||
		{ echo "FAIL page prefix template"; tail -5 "$w/page-pfx.log"; exit 1; }
	prefix=$w/page-pfx
	tag="W64 page $page"
	# the door off in the single-thread build (it has none: the pipes)
	[ "$page" = sp ] && door=0
fi
if [ -n "${NTDLL:-}${NATIVE:-}" ]; then
	# DIST with those DLLs: the Unix side copied (the loader finds the PE
	# DLLs beside itself), links to the other DLLs
	[ -z "${NTDLL:-}" ] || [ -f "$NTDLL" ] || { echo "no ntdll at $NTDLL" >&2; exit 2; }
	[ -z "${NATIVE:-}" ] || [ -f "$NATIVE/winepe.wasm" ] || { echo "no winepe.wasm in $NATIVE" >&2; exit 2; }
	o=$w/dist; rm -rf "$o"; mkdir -p "$o/lib/wine/x86_64-windows"
	cp -a "$dist/lib/wine/x86_64-unix" "$o/lib/wine/"
	if [ -n "$page" ]; then
		# the page's mount does not follow a link out of it: hard links
		cp -al "$dist/share" "$o/share"; ln -s ../../share "$o/lib/wine/share"
		cp -al "$dist"/lib/wine/x86_64-windows/. "$o/lib/wine/x86_64-windows/"
	else
		ln -s "$dist/share" "$o/share"; ln -s ../../share "$o/lib/wine/share"
		for f in "$dist"/lib/wine/x86_64-windows/*; do ln -s "$f" "$o/lib/wine/x86_64-windows/"; done
	fi
	if [ -n "${NTDLL:-}" ]; then
		rm "$o/lib/wine/x86_64-windows/ntdll.dll"; cp "$NTDLL" "$o/lib/wine/x86_64-windows/ntdll.dll"
		tag="W64 native ntdll"
	fi
	if [ -n "${NATIVE:-}" ]; then
		for f in "$NATIVE"/*; do
			rm -f "$o/lib/wine/x86_64-windows/${f##*/}"; cp "$f" "$o/lib/wine/x86_64-windows/"
		done
		tag="W64 native $(cd "$NATIVE" && ls -- *.dll *.drv *.exe 2>/dev/null | sed 's/\.[^.]*$//' | tr '\n' ' ' | sed 's/ $//')"
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
	# (the display and GL cases are native only for now)
	[ -n "$page" ] && [ -n "${WL:-}${GL:-}" ] && return
	local p=$w/pfx-$name out code src=$bin/$exe
	case $exe in /*) src=$exe; exe=$(basename "$exe") ;; esac
	rm -rf "$p"; cp -a "$prefix" "$p"; cp "$src" ${EXTRA:-} "$p/drive_c/"
	local wl=()
	if [ -n "${WL:-}" ]; then
		# the Wayland driver, gexc's compositor, its frames as PPMs; display
		# modes emulated (Wayland sets none), as W3's game prefix has them
		printf '\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="wayland"\n' >> "$p/user.reg"
		printf '\n[Software\\\\Wine\\\\X11 Driver] 1791289000\n"EmulateModeset"="Y"\n' >> "$p/user.reg"
		# the prefix is made without a display (w64-dist.sh: native
		# wineboot, no Wayland libraries): the driver's file, as wineboot
		# would install it
		[ -e "$p/drive_c/windows/system32/winewayland.drv" ] ||
			cp "$dist/lib/wine/x86_64-windows/winewayland.drv" "$p/drive_c/windows/system32/"
		rm -f "$w/$name"-*.ppm
		wl=(--wayland "$w/wl-$name" --wayland-size "${WL_SIZE:-640x480}" --wayland-out "$w/$name")
	fi
	local gl=(); [ -z "${GL:-}" ] || gl=(--gl)
	if [ -n "$page" ]; then
		out=$(cd "$root" && env ${single:+GEX_SINGLE=1} GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" \
			node $jspi --max-old-space-size=8192 \
			--import 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer' \
			shims/browser/js/run-node.mjs --dir /w64="$dist" --dir /pfx="$p" \
			--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=$door \
			--cwd /pfx/drive_c --argv0 /w64/lib/wine/x86_64-unix/wine \
			"$seed" "$wine" "C:\\$exe" "$@" 2>"$w/$name.err"; echo "exit=$?")
	else
	out=$(cd "$p/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run "${gl[@]}" "${wl[@]}" \
		--env WINEPREFIX="$p" --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=$door \
		"$wine" "C:\\$exe" "$@" 2>"$w/$name.err"; echo "exit=$?")
	fi
	code=${out##*exit=}
	out=${out%$'\n'exit=*}; out=${out%exit=*}; out=${out//$'\r'/}
	# The page's space ends at 2 GiB: a Wine that takes it for 4 GiB tries
	# the top first for each thread's stack, ENOMEM and an ERR each time
	# (Wine fl/wine-addrlimit probes 2 GiB too). Quiet logs: none at all.
	if [ -n "$page" ] && grep -aq "mmap() error Out of memory" "$w/$name.err"; then
		code="$code, $(grep -ac "mmap() error Out of memory" "$w/$name.err") mmap ENOMEM ERR lines"
	fi
	local fr=""
	# (WL=- : the display without a frame check)
	[ -z "${WL:-}" ] || [ "$WL" = - ] || fr=$(python3 "$here/frame.py" "$w/$name" "${WL_MIN:-1000}" $WL 2>&1) || code="$code, $fr"
	if [ "$code" = "$want_code" ] && [[ "$out" =~ ^${want_out}$ ]]; then
		pass=$((pass + 1)); echo "ok   x64/$name ($tag)"
	else
		fail=$((fail + 1)); echo "FAIL x64/$name ($tag): exit $code (want $want_code)"
		echo "  stdout: $(printf %q "$out" | cut -c1-400)"
		grep -v '^\[host\]' "$w/$name.err" | tail -8 | sed 's/^/  stderr: /'
	fi
	rm -rf "$p" "$w/wl-$name"; rm -f "$w/$name"-*.ppm
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
# W64.4: a window painted with GDI, shown by the Wayland driver (for 3 s)
if grep -q winewayland.so "$wine" && [ -e "$dist/lib/wine/x86_64-windows/winewayland.drv" ]; then
	WL="220,40,40 40,200,60 40,80,220 230,210,40 255,255,255" check window 0 "shown${nl}bye" window.exe 3
	# Wine's x64 notepad, up for 5 s and closed by WM_CLOSE (notepadrun.c):
	# its caption, edit field, and the text of title, menu bar and status
	# bar (black: about 740 pixels, 335 of them the menus')
	WL="50,150,250:5000 255,255,255:50000 0,0,0:600" check notepad 0 "title [^$nl]*Notepad${nl}closed 0" notepadrun.exe 5
	# tests/nt's GUI programs on the display (they check themselves);
	# display.exe fails 2 checks: a named section mapped twice in one
	# process does not share its pages (wasm memory has no aliases; the
	# kernel hands a second MAP_SHARED the first's address, and MAP_FIXED
	# views cannot have it). x86/display fails under W3 too (known-fail,
	# w3_boot). Its 800x600 must fit the output: emulated modes go up to
	# the output's size.
	for t in user_win gdi input; do WL=- check $t 0 "$allok" $t.exe; done
	WL=- WL_SIZE=1024x768 check display 0 "$allok" display.exe
	# W64.4: OpenGL, when the loader has opengl32.so and gex-gl's EGL
	# (link-w64.sh): the host's GL (gexc run --gl). glwindow draws the four
	# quadrants and the white square with GL for 3 s, its frames checked.
	if grep -q libEGL.so.1 "$wine"; then
		for t in glcompat glfast; do GL=1 WL=- check $t 0 "$allok" $t.exe; done
		GL=1 WL="220,40,40 40,200,60 40,80,220 230,210,40 255,255,255:100" \
			check glwindow 0 "shown${nl}frames [0-9]+${nl}bye" glwindow.exe 3
	fi
fi
# Audio: waveOut and an mmdevapi stream each play 100 ms (audio.c) through
# wineoss.drv's Unix side in the loader (link-w64.sh) to gex-kernel's
# /dev/dsp, natively and in the page. A loader without it fails the case:
# every x64 game that opens an audio device stops there (OneShot's OpenAL).
# Not yet PAGE=sp: there, after waveOut, the process outlives its exit (the
# server's SIGKILL; mmdevapi alone exits), so the case times out.
if ! grep -q wineoss.so "$wine"; then
	case " ${CASES:-audio} " in *" audio "*)
		fail=$((fail + 1)); echo "FAIL x64/audio ($tag): the loader has no wineoss.so (link-w64.sh)" ;; esac
elif x86_64-w64-mingw32-gcc -O1 -o "$w/audio.exe" "$here/audio.c" -lwinmm -lole32; then
	check audio 0 "waveout ok${nl}mmdevapi ok" "$w/audio.exe"
else
	fail=$((fail + 1)); echo "FAIL x64/audio ($tag): x86_64-w64-mingw32-gcc"
fi
# crypt32: a signed message whose authenticated attributes are not in DER
# order verifies (authattrs.c; vectors from authattrs-gen.py). FL Studio's
# check of its engine DLL's Authenticode signature needs it.
if x86_64-w64-mingw32-gcc -O1 -o "$w/authattrs.exe" "$here/authattrs.c" -lcrypt32; then
	check authattrs 0 "$allok" "$w/authattrs.exe"
else
	case " ${CASES:-authattrs} " in *" authattrs "*)
		fail=$((fail + 1)); echo "FAIL x64/authattrs ($tag): x86_64-w64-mingw32-gcc" ;; esac
fi
# W64.1b: data above 2 GiB, code below it (highdata.c). Not in the page:
# there the shim's heap is the upper 2 GiB of the process's memory, and the
# guest gets just under 2 GiB (GUEST_LIMIT 0x7fff0000; about 1975 MiB
# allocates). It needs the memory64 page (W64.7).
if [ -n "$page" ]; then
	case " ${CASES:-highdata} " in *" highdata "*)
		echo "skip x64/highdata ($tag): the page's guest has under 2 GiB until W64.7" ;; esac
elif x86_64-w64-mingw32-gcc -O1 -o "$w/highdata.exe" "$here/highdata.c"; then
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
