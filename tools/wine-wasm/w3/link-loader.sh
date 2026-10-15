#!/bin/bash
# Link W3's wine loader: loader/main.o, ntdll's Unix side and every Unix
# library a PE module calls, in one wasm32 program (a closed world,
# docs/wine-native.md §3.12): each library's objects compiled again with its
# __wine_unix_call_funcs (and names it shares with another) renamed apart,
# and a registry (dlreg.py) that musl's dlopen/dlsym answer from. The
# libraries: crypt32, ws2_32, msv1_0, dnsapi, nsiproxy, win32u (+ FreeType),
# winewayland.drv (+ libwayland, xkbcommon), opengl32 (+ gex-gl's EGL),
# wineoss.drv (OSSv4, against gex-kernel's /dev/dsp).
#
#   w3/link-loader.sh BUILD SRC OBJ DEPS GL_O DIST
#
# BUILD  the configured, built wasm32 tree (w3/wine-build.sh wasm): the
#        compile commands come from its Makefile (make -n), the objects that
#        need no renaming from the tree itself
# SRC    the Wine source BUILD was configured from
# OBJ    where the renamed objects go (reused: one is compiled again when its
#        source or this script is newer)
# DEPS   w3/deps.sh's output; GL_O gex-gl's wasm32 object (guest/gl/gexgl.c)
# DIST   writes DIST/lib/wine/i386-unix/wine (and wine.wasm)
# Env: GEX_WASM_SYSROOT, W3_JOBS.
set -u
here=$(cd "$(dirname "$0")" && pwd); T=$(cd "$here/.." && pwd)
B=$(realpath "${1:?BUILD}") F=$(realpath "${2:?SRC}") O=$(realpath -m "${3:?OBJ}") D=$(realpath "${4:?DEPS}")
GL_O=$(realpath "${5:?GL_O}") DIST=$(realpath -m "${6:?DIST}")
: "${GEX_WASM_SYSROOT:?}"
J=${W3_JOBS:-12}
I=$D/install
mkdir -p "$O"; rm -f "$O/.fail"; cd "$B" || exit 1
WF=$(sed -n 's/^srcdir *= *//p' Makefile); WF=$(cd "$WF" && pwd)
compile_cmd() { # SRC_PATH OBJ_REL -> the build's compile command for it
  make -n -W "$1" "$2" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $2"
}
objs=""
# compile SRC_REL OBJ_REL OUT DEFS [CMD]: OUT from the build's command for
# OBJ_REL (or CMD), with DEFS, unless OUT is newer than its source and this
# script; in the background, at most W3_JOBS at once.
compile() {
  local src=$F/$1 rel=$2 out=$3 defs=$4 cmd=${5:-}
  objs="$objs $out"
  [ "$out" -nt "$src" ] && [ "$out" -nt "$here/link-loader.sh" ] && return
  [ -n "$cmd" ] || cmd=$(compile_cmd "$WF/$1" "$rel")
  [ -n "$cmd" ] || { echo "link-loader: no compile command for $rel" >&2; touch "$O/.fail"; return; }
  cmd=${cmd//-c -o $rel/-c -o $out}; cmd=${cmd//$WF/$F}
  ( eval "$cmd $defs" ) || { echo "link-loader: $1 failed" >&2; touch "$O/.fail"; } &
  while [ "$(jobs -r | wc -l)" -ge "$J" ]; do wait -n; done
}
for c in $(grep -o "dlls/ntdll/unix/[a-z_0-9]*\.o" Makefile | sort -u | grep -v "signal_arm\|signal_i386\|signal_x86"); do
  objs="$objs $B/$c"
done
reg=""
for spec in crypt32:crypt32 ws2_32:ws2_32 msv1_0:msv1_0 dnsapi:dnsapi nsiproxy.sys:nsiproxy; do
  dir=${spec%%:*}; lib=${spec##*:}; mkdir -p "$O/$lib"
  for c in $(sed -n "/^dlls\/$dir\/$lib.so:/,/^\t/p" Makefile | grep -o "dlls/$dir/[a-z_0-9]*\.o" | sort -u); do
    b=$(basename "$c" .o); compile "dlls/$dir/$b.c" "$c" "$O/$lib/$b.o" "-D__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
  done
  reg="$reg $lib.so:data:__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
done
# win32u.so: its syscalls register at __wine_unix_lib_init; the names it
# shares with wineserver and ntdll.so renamed apart. FreeType through dlopen.
w32defs="-D__wine_unix_lib_init=win32u___wine_unix_lib_init"
for n in alloc_user_handle destroy_thread_windows free_user_handle get_virtual_screen_rect get_window_thread init_startup_info is_desktop_class is_message_class is_window_visible mirror_region send_notify_message shared_session; do
  w32defs="$w32defs -D$n=win32u_$n"
done
for f in $(grep -o "LOAD_FUNCPTR(FT_[A-Za-z_]*)" "$F/dlls/win32u/freetype.c" | sort -u | sed 's/LOAD_FUNCPTR(\(.*\))/\1/'); do reg="$reg libfreetype.so.6:func:$f"; done
mkdir -p "$O/win32u"
for c in $(sed -n "/^dlls\/win32u\/win32u.so:/,/^\t/p" Makefile | grep -o "dlls/win32u/[a-z_0-9/]*\.o" | sort -u); do
  rel=${c#dlls/win32u/}; compile "dlls/win32u/${rel%.o}.c" "$c" "$O/win32u/${rel//\//_}" "$w32defs"
done
reg="$reg win32u.so:func:__wine_unix_lib_init=win32u___wine_unix_lib_init"
# winewayland.so, the display driver.
wldefs="-D__wine_unix_call_funcs=winewayland__wine_unix_call_funcs -Dget_region_data=winewayland_get_region_data"
mkdir -p "$O/winewayland"
for c in $(sed -n "/^dlls\/winewayland.drv\/winewayland.so:/,/^\t/p" Makefile | grep -o "dlls/winewayland.drv/[a-z_0-9-]*\.o" | sort -u); do
  b=$(basename "$c" .o)
  if [ -f "$F/dlls/winewayland.drv/$b.c" ]; then compile "dlls/winewayland.drv/$b.c" "$c" "$O/winewayland/$b.o" "$wldefs"
  else
    # a generated protocol source: built in the tree, then renamed apart
    make "$c" > /dev/null 2>&1
    cmd=$(make -n -W "dlls/winewayland.drv/$b.c" "$c" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $c")
    objs="$objs $O/winewayland/$b.o"
    ( eval "${cmd//-c -o $c/-c -o $O/winewayland/$b.o} $wldefs" ) || { echo "link-loader: $c failed" >&2; touch "$O/.fail"; }
  fi
done
reg="$reg winewayland.so:data:__wine_unix_call_funcs=winewayland__wine_unix_call_funcs"
wllibs="$I/lib/libwayland-egl.a $I/lib/libwayland-client.a $I/lib/libxkbcommon.a $I/lib/libxkbregistry.a $I/lib/libxml2.a"
# opengl32.so: win32u's EGL is gex-gl's, found by dlopen("libEGL.so.1"); gex-gl
# finds libwayland-client the same way (RTLD_NOLOAD).
mkdir -p "$O/opengl32"
for c in $(sed -n "/^dlls\/opengl32\/opengl32.so:/,/^\t/p" Makefile | grep -o "dlls/opengl32/[a-z_0-9]*\.o" | sort -u); do
  b=$(basename "$c" .o); compile "dlls/opengl32/$b.c" "$c" "$O/opengl32/$b.o" "-D__wine_unix_call_funcs=opengl32__wine_unix_call_funcs"
done
reg="$reg opengl32.so:data:__wine_unix_call_funcs=opengl32__wine_unix_call_funcs"
for f in $(llvm-nm --defined-only --extern-only "$GL_O" | awk '$2 == "T" { print $3 }'); do reg="$reg libEGL.so.1:func:$f"; done
for f in wl_display_flush wl_proxy_destroy wl_proxy_get_display wl_proxy_get_id wl_proxy_marshal wl_proxy_marshal_constructor; do reg="$reg libwayland-client.so.0:func:$f"; done
reg="$reg libwayland-client.so.0:data:wl_callback_interface"
# wineoss.so (audio), a Unix library only: configure leaves it out (no OSSv4
# header in the sysroot), so it is compiled like crypt32's unixlib, against
# DEPS' sys/soundcard.h.
mkdir -p "$O/wineoss"
tmpl=$(compile_cmd "$WF/dlls/crypt32/unixlib.c" dlls/crypt32/unixlib.o)
for b in midipatch oss ossmidi; do
  cmd=${tmpl//dlls\/crypt32\/unixlib.o/dlls\/wineoss.drv\/$b.o}; cmd=${cmd//$WF\/dlls\/crypt32\/unixlib.c/$F\/dlls\/wineoss.drv\/$b.c}
  cmd=${cmd//-Idlls\/crypt32/-Idlls\/wineoss.drv}; cmd=${cmd//-I$WF\/dlls\/crypt32/-I$F\/dlls\/wineoss.drv -I$I\/oss}; cmd=${cmd//-D_CRYPT32_/}
  compile "dlls/wineoss.drv/$b.c" "dlls/wineoss.drv/$b.o" "$O/wineoss/$b.o" "-D__wine_unix_call_funcs=wineoss__wine_unix_call_funcs" "$cmd"
done
reg="$reg wineoss.so:data:__wine_unix_call_funcs=wineoss__wine_unix_call_funcs"
wait
[ -e "$O/.fail" ] && { echo "link-loader: compile failed; not linking" >&2; exit 1; }
python3 "$T/dlreg.py" ntdll.so:func:__wine_main $reg > "$O/reg.c"
mkdir -p "$DIST/lib/wine/i386-unix"
"$T/wcc" -O1 -Wl,--gex-guest-i386 -o "$DIST/lib/wine/i386-unix/wine" "$B/loader/main.o" "$O/reg.c" $objs \
  "$I/lib/libfreetype.a" $wllibs "$GL_O" || { echo "link-loader: link failed" >&2; exit 1; }
echo "w3/link-loader: $DIST/lib/wine/i386-unix/wine"
