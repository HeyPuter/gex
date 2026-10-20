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
# MEM64=1 runs them in the memory64 flavour (W64.7, §0.1), in the page only
# (PAGE=mp): DIST is then a W64_MEM64=1 install (w64-dist.sh), run on the
# 64-bit memory's shim and seed (GEX_MEM64=1 shims/browser/build.sh, the
# page's ?mem64=1) with the wasm32 server in a 32-bit memory beside it;
# highdata then allocates 5 GiB (`highdata mem64`). Not natively: a gexc
# run's processes share one flavour (OPTIMIZATION-BACKLOG.md).
# TWIN=1 (PAGE=mp) runs them per program instead (docs/mem64.md §5): the
# default 32-bit page and a W64_TWIN=1 install, each case's loader exec'd by
# a launcher with GEX_MEM64=1, which runs the install's memory64 twin
# (wine.m64) in a 64-bit memory; the server, and the launcher, stay 32-bit.
# highdata allocates 5 GiB as under MEM64=1.
# Its `image` case is a program's own record instead (the Hades case): the
# user.gex.mem64 attribute on one .exe, its parent, its child and its
# sibling 32-bit (Wine's WINEGEX_IMAGE names the .exe at the exec).
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
m64=; [ "${MEM64:-0}" = 1 ] && m64=1
[ -z "$m64" ] || [ "${PAGE:-}" = mp ] || { echo "MEM64=1 runs in the page: PAGE=mp" >&2; exit 2; }
twin=; [ "${TWIN:-0}" = 1 ] && twin=1
[ -z "$twin" ] || { [ "${PAGE:-}" = mp ] && [ -z "$m64" ]; } || { echo "TWIN=1 runs in the default page: PAGE=mp, no MEM64" >&2; exit 2; }
[ -z "$twin" ] || [ -f "$dist/lib/wine/x86_64-unix/wine.m64" ] || { echo "no wine.m64 in $dist: a W64_TWIN=1 install" >&2; exit 2; }
w=${WORK:-$(mktemp -d)}
mkdir -p "$w"
tag=W64${m64:+ mem64}
page=${PAGE:-}
if [ -n "$page" ]; then
	case $page in mp|sp) ;; *) echo "PAGE is mp or sp" >&2; exit 2 ;; esac
	[ -z "${GL:-}" ] || { echo "PAGE runs without GL" >&2; exit 2; }
	seed=$root/web/public/gex/seed.x86.shared.wasm
	[ -f "$seed" ] && [ -f "$root/web/public/gex/gex_browser.x86.shared.wasm" ] ||
		{ echo "no x86 shared shim and seed in web/public/gex; GEX_ARCH=x86 shims/browser/build.sh" >&2; exit 2; }
	if [ -n "$m64" ]; then
		seed=$root/web/public/gex/seed.x86.m64.shared.wasm
		[ -f "$seed" ] && [ -f "$root/web/public/gex/gex_browser.x86.m64.shared.wasm" ] ||
			{ echo "no x86 m64 shim and seed in web/public/gex; GEX_ARCH=x86 GEX_MEM64=1 shims/browser/build.sh" >&2; exit 2; }
	fi
	# the single-thread build needs JSPI (on by default from node 25)
	jspi=; node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null || jspi=--experimental-wasm-jspi
	"$root/tools/wine-wasm/page-prefix.sh" "$prefix" "$dist" "$w/page-pfx" > "$w/page-pfx.log" 2>&1 ||
		{ echo "FAIL page prefix template"; tail -5 "$w/page-pfx.log"; exit 1; }
	prefix=$w/page-pfx
	tag="W64 page $page${m64:+ mem64}${twin:+ twin}"
	if [ -n "$twin" ]; then
		# the launcher: the loader exec'd with GEX_MEM64=1 (the kernel takes
		# it out again), so the twin runs in a 64-bit memory. `-x FILE`
		# instead: FILE's user.gex.mem64 record set to 1, and the loader
		# exec'd with nothing asked (a Windows program's own record, which
		# Wine names to the kernel at its exec: WINEGEX_IMAGE)
		printf '%s\n' '#include <stdio.h>' '#include <stdlib.h>' '#include <string.h>' '#include <unistd.h>' '#include <sys/xattr.h>' \
			'int main(int c, char **v) {' \
			'  if (c > 3 && !strcmp(v[1], "-x")) {' \
			'    if (setxattr(v[2], "user.gex.mem64", "1", 1, 0)) { perror("setxattr"); return 126; }' \
			'    v += 2;' \
			'  } else setenv("GEX_MEM64", "1", 1);' \
			'  execv(v[1], v + 1); return 127; }' > "$w/launch.c"
		zig cc -target x86_64-linux-musl -O1 -static -o "$w/launch" "$w/launch.c" || { echo "FAIL the twin's launcher (zig)"; exit 1; }
	fi
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
	# RSS_OUT=FILE: the page run's peak RSS (KiB, the node process and its Workers) into FILE
	local rss=()
	[ -n "${RSS_OUT:-}" ] && rss=(python3 -c 'import resource,subprocess,sys; rc=subprocess.call(sys.argv[2:]); open(sys.argv[1],"w").write("%d\n" % resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss); sys.exit(rc)' "$RSS_OUT")
	if [ -n "$page" ]; then
		# the loader as the process, or exec'd by the twin's launcher
		local run=(--argv0 /w64/lib/wine/x86_64-unix/wine "$seed" "$wine")
		[ -z "$twin" ] || run=(--file /launch="$w/launch" --argv0 /launch "$seed" "$w/launch" ${MARK:+-x "/pfx/drive_c/$MARK"} /w64/lib/wine/x86_64-unix/wine)
		# DIRECT=1: the loader as the process, with GEX_MEM64=1 in its
		# environment (a game's page that asks for it so)
		[ "${DIRECT:-0}" = 0 ] || run=(--env GEX_MEM64=1 --argv0 /w64/lib/wine/x86_64-unix/wine "$seed" "$wine")
		# UNIX=1: the program named by its Unix path (wine /game/x.exe)
		local prog="C:\\$exe"; [ "${UNIX:-0}" = 0 ] || prog=/pfx/drive_c/$exe
		out=$(cd "$root" && env ${single:+GEX_SINGLE=1} ${m64:+GEX_MEM64=1} GEX_ARCH=x86 GEX_SHARED=1 "${rss[@]}" timeout -k 10 "${TMO:-300}" \
			node $jspi --max-old-space-size=8192 \
			--import 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer' \
			shims/browser/js/run-node.mjs --dir /w64="$dist" --dir /pfx="$p" \
			--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=$door \
			--cwd /pfx/drive_c "${run[@]}" "$prog" "$@" 2>"$w/$name.err"; echo "exit=$?")
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
# order verifies (authattrs.c; vectors from authattrs-gen.py), as an
# Authenticode check of a signed DLL needs.
if x86_64-w64-mingw32-gcc -O1 -o "$w/authattrs.exe" "$here/authattrs.c" -lcrypt32; then
	check authattrs 0 "$allok" "$w/authattrs.exe"
else
	case " ${CASES:-authattrs} " in *" authattrs "*)
		fail=$((fail + 1)); echo "FAIL x64/authattrs ($tag): x86_64-w64-mingw32-gcc" ;; esac
fi
# Images loaded, run and unloaded one after another (dllcycle.c: 24 distinct
# DLLs of ~1000 functions each, dllcycle-gen.py), as a plugin scan does: each
# one's translations must go with it once it is unloaded. In the page, the
# run's peak RSS grows by under DLLCYCLE_MB (default 18) MiB per further
# unloaded image. The slope is taken between two long runs, the 24 images
# once and RSS_ROUNDS (default 4) times (each round loads every image again
# at its base, translated again): the peak's own swing between runs, a few
# percent of a large total, is then spread over 72 further loads, and both
# ends carry the same baseline. Each load currently keeps about 13 MiB; the
# bound catches growth beyond that.
# ROUNDS=2 loads each again at the same base (translated again).
if x86_64-w64-mingw32-gcc -O1 -o "$w/dllcycle.exe" "$here/dllcycle.c" &&
	python3 "$here/dllcycle-gen.py" 1000 "$w/bigcode.c" &&
	x86_64-w64-mingw32-gcc -O1 -c -o "$w/bigcode.o" "$w/bigcode.c" &&
	for i in $(seq -w 0 23); do
		x86_64-w64-mingw32-gcc -shared -o "$w/plug$i.dll" "$w/bigcode.o" \
			-Wl,--image-base=$(printf '0x%x' $((0x300000000 + 10#$i * 0x1000000))) || exit 1
	done; then
	plugs=$(ls "$w"/plug*.dll | tr '\n' ' ')
	# x as the plugins' code computes it (each image's code runs right after a reload too)
	x4=$(python3 "$here/dllcycle-gen.py" 1000 --expect 4)
	x24=$(python3 "$here/dllcycle-gen.py" 1000 --expect 24)
	xr=$(python3 "$here/dllcycle-gen.py" 1000 --expect $((24 * ${ROUNDS:-1})))
	if [ -n "$page" ] && case " ${CASES:-dllcycle} " in *" dllcycle "*) true ;; *) false ;; esac; then
		f0=$fail; R=${RSS_ROUNDS:-4}
		xR=$(python3 "$here/dllcycle-gen.py" 1000 --expect $((24 * R)))
		EXTRA="$plugs" RSS_OUT="$w/rss1" CASES=dllcycle check dllcycle 0 "dllcycle 24 x $x24" "$w/dllcycle.exe" 24 1
		EXTRA="$plugs" RSS_OUT="$w/rssR" CASES=dllcycleR TMO=${TMO:-$((300 * R))} check dllcycleR 0 "dllcycle $((24 * R)) x $xR" "$w/dllcycle.exe" 24 $R
		if [ "$fail" = "$f0" ] && [ -s "$w/rss1" ] && [ -s "$w/rssR" ]; then
			# KiB difference over the further loads, in MiB (one decimal kept for the log)
			per10=$(( ($(cat "$w/rssR") - $(cat "$w/rss1")) * 10 / (24 * (R - 1)) / 1024 ))
			per="$((per10 / 10)).$(( (per10 < 0 ? -per10 : per10) % 10 ))"
			if [ "$per10" -lt "$(( ${DLLCYCLE_MB:-18} * 10 ))" ]; then
				pass=$((pass + 1)); echo "ok   x64/dllcycle-rss ($tag): $per MiB per further unloaded image (rss $(cat "$w/rss1") -> $(cat "$w/rssR") KiB, 24 -> $((24 * R)) loads)"
			else
				fail=$((fail + 1)); echo "FAIL x64/dllcycle-rss ($tag): $per MiB per further unloaded image (bound ${DLLCYCLE_MB:-18}; rss $(cat "$w/rss1") -> $(cat "$w/rssR") KiB, 24 -> $((24 * R)) loads)"
			fi
		fi
	else
		EXTRA="$plugs" check dllcycle 0 "dllcycle 24 x $x24" "$w/dllcycle.exe" 24 1
	fi
else
	case " ${CASES:-dllcycle} " in *" dllcycle "*)
		fail=$((fail + 1)); echo "FAIL x64/dllcycle ($tag): x86_64-w64-mingw32-gcc" ;; esac
fi
# W64.1b: data above 2 GiB, code below it (highdata.c). Not in a 32-bit
# memory's page: there the shim's heap is the upper 2 GiB of the process's
# memory, and the guest gets just under 2 GiB (GUEST_LIMIT 0x7fff0000;
# about 1975 MiB allocates). In the memory64 page (MEM64=1) 5 GiB, code
# anywhere (`highdata mem64`). So per program (TWIN=1).
if [ -n "$m64$twin" ]; then
	if x86_64-w64-mingw32-gcc -O1 -o "$w/highdata.exe" "$here/highdata.c"; then
		check highdata 0 "$allok" "$w/highdata.exe" mem64
	else
		fail=$((fail + 1)); echo "FAIL x64/highdata ($tag): x86_64-w64-mingw32-gcc"
	fi
elif [ -n "$page" ]; then
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
# A skeleton's view unmapped and its addresses reused for the program's own
# code (stalebind.c): the native bindings must not outlive the view. Only
# with the native PE core, whose DLLs are skeletons (NATIVE=).
if [ -n "${NATIVE:-}" ]; then
	if x86_64-w64-mingw32-gcc -O1 -o "$w/stalebind.exe" "$here/stalebind.c"; then
		check stalebind 0 "$allok" "$w/stalebind.exe"
	else
		fail=$((fail + 1)); echo "FAIL x64/stalebind ($tag): x86_64-w64-mingw32-gcc"
	fi
fi
# TWIN=1: the 64-bit program's own child (CreateProcess, which Wine execs
# from its loader, so wine.m64) is a 32-bit process again, run by `wine`:
# highdata's 5 GiB, which its parent could have, fail in it.
if [ -n "$twin" ] && [ -f "$w/highdata.exe" ]; then
	EXTRA="$w/highdata.exe" check childtwin 4 "([^$nl]*$nl)*FAIL data above 4 GiB$nl([^$nl]*$nl)*child exit 4" childcon.exe "highdata.exe mem64"
fi
# TWIN=1, the Hades case: a Windows program's own record (user.gex.mem64 on
# its .exe; Wine names the .exe to the kernel at the loader's exec) makes it
# alone a 64-bit process. Started by a 32-bit parent (siblings.exe, nothing
# asked), hades.exe (highdata.exe under another name, the record on it) has
# 5 GiB; its own child, highdata.exe, and its sibling, highdata.exe again,
# are 32-bit processes (`highdata where`: a top-down allocation below 4 GiB).
if [ -n "$twin" ] && [ -f "$w/highdata.exe" ] && x86_64-w64-mingw32-gcc -O1 -o "$w/siblings.exe" "$here/siblings.c"; then
	cp "$w/highdata.exe" "$w/hades.exe"
	MARK=hades.exe EXTRA="$w/highdata.exe $w/hades.exe" check image 0 \
		"(ok [^$nl]*$nl)+0 failures${nl}flavour 32${nl}child exit 0${nl}child exit 0${nl}flavour 32${nl}child exit 0" \
		"$w/siblings.exe" "hades.exe mem64 highdata.exe where" "highdata.exe where"
elif [ -n "$twin" ]; then
	case " ${CASES:-image} " in *" image "*) fail=$((fail + 1)); echo "FAIL x64/image ($tag): build" ;; esac
fi
# TWIN=1, a game started directly (game.json exe=wine, argv the .exe): the
# loader is the first process, and the program runs in its relaunch (the
# server starter's child, Wine loader.c wineserver_relaunch).
# - `direct-env`: GEX_MEM64=1 in the loader's environment; the relaunch
#   asks for a 64-bit memory, so it runs wine.m64 (vfork.rs `exec_route`).
# - `direct-image`: the .exe named by its Unix path, with its record
#   (user.gex.mem64), nothing else asked; the relaunch names it
#   (WINEGEX_IMAGE), so it alone is 64-bit.
if [ -n "$twin" ] && [ -f "$w/highdata.exe" ]; then
	DIRECT=1 check direct-env 0 "flavour 64" "$w/highdata.exe" where
	cp "$w/highdata.exe" "$w/hades.exe"
	UNIX=1 MARK=hades.exe check direct-image 0 "flavour 64" "$w/hades.exe" where
fi

# W64.2: a native DLL (a lowered wasm64 module behind a PE32+ skeleton)
# called from x64 code through every kind of Win64 entry adapter, and
# calling back (tests/wasm64/dll)
if [ -n "${W64_TOOLS:-}" ] && [ -n "${W64_SRC:-}" ]; then
	case " ${CASES:-dll} " in *" dll "*)
		# wcc64 needs the sysroot (build-sysroot64.sh) as well as the tools.
		# Not beside the native PE core: a process has one Wine PE module
		# (wasmdll.rs WINE_PE), so abi.dll's skeleton would be bound into
		# winepe.wasm's slots, and LoadLibrary runs its ud2 stubs (c000001d).
		if [ -n "${NATIVE:-}" ]; then
			echo "skip x64/dll ($tag): abi.dll's own module cannot load beside the native PE core (one Wine PE module a process)"
		elif [ -z "${GEX_WASM64_SYSROOT:-}" ]; then
			fail=$((fail + 1)); echo "FAIL x64/dll (W64): build: GEX_WASM64_SYSROOT is not set (W64_TOOLS and W64_SRC are)"
		elif "$root/tests/wasm64/dll/build.sh" "$w/dll" "$W64_SRC" "$W64_TOOLS" > "$w/dll.build" 2>&1; then
			EXTRA="$w/dll/abi.dll" check dll 0 "(ok [^$nl]*$nl)+PASS" "$w/dll/abi_test.exe"
		else
			# The last line on the FAIL line itself: a caller keeping only
			# ok/FAIL lines still sees why.
			fail=$((fail + 1)); echo "FAIL x64/dll (W64): build: $(tail -1 "$w/dll.build")"; tail -5 "$w/dll.build" | sed 's/^/  /'
		fi ;;
	esac
fi
[ -n "${WORK:-}" ] || rm -rf "$w"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
