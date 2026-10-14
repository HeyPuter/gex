#!/bin/bash
# 14: the W3 wine loader, lib/wine/i386-unix/wine of the dist: loader/main.o
# and ntdll's Unix side, with the wineserver linked in and run as a thread
# (server/wasm32_inproc.h), and the Unix libraries of the PE modules W3 uses
# compiled again with their unix-call tables renamed apart and registered
# for musl's dlopen (tools/wine-wasm/dlreg.py): crypt32, ws2_32, msv1_0,
# dnsapi, nsiproxy, mountmgr (the shell's drives and volumes), win32u (with
# FreeType), winewayland (with its Wayland libraries), opengl32 (with
# gex-gl's EGL) and wineoss. Objects go to wine-wasm/gexw3.
# Output: $W7_OUT/dist/lib/wine/i386-unix/wine.
. "$(dirname "$0")/lib.sh"; need WINE_SRC
F=$(cd "$WINE_SRC" && pwd); B=$W7_OUT/wine-wasm; D=$W7_OUT/deps; T=$GEX_SRC/tools/wine-wasm; L=$W7_OUT/logs
DIST=$W7_OUT/dist; mkdir -p "$DIST/lib/wine/i386-unix"
cd "$B"
step "the loader"
exec 3> "$L/link-loader.log"
# compile SRC OBJ_TARGET OUT EXTRA: OBJ_TARGET's command from the Makefile, into OUT, with EXTRA
compile() {
  local cmd
  cmd=$(make -n -W "$1" "$2" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $2")
  [ -n "$cmd" ] || { echo "no command for $2" >&2; exit 1; }
  eval "nice $cmd $4" -o "$3" >&3 2>&1 || { echo "compiling $1 failed (logs/link-loader.log)" >&2; exit 1; }
}
objs=$(grep -o "dlls/ntdll/unix/[a-z_0-9]*\.o" Makefile | sort -u | grep -v "signal_arm\|signal_i386\|signal_x86" | tr '\n' ' ')
make -j"$JOBS" loader/main.o $objs >&3 2>&1
mkdir -p gexw3/server
for c in $(sed -n '/^server\/wineserver:/,/^\t/p' Makefile | grep -o "server/[a-z_0-9]*\.o" | sort -u); do
  b=${c#server/}; compile "$F/server/${b%.o}.c" "$c" "gexw3/server/$b" "-include $F/server/wasm32_inproc.h"
  objs="$objs gexw3/server/$b"
done
reg=""
for spec in crypt32:crypt32 ws2_32:ws2_32 msv1_0:msv1_0 dnsapi:dnsapi nsiproxy.sys:nsiproxy mountmgr.sys:mountmgr; do
  dir=${spec%%:*}; lib=${spec##*:}; mkdir -p "gexw3/unix/$lib"
  for c in $(sed -n "/^dlls\/$dir\/$lib.so:/,/^\t/p" Makefile | grep -o "dlls/$dir/[a-z_0-9]*\.o" | sort -u); do
    b=$(basename "$c" .o); compile "$F/dlls/$dir/$b.c" "$c" "gexw3/unix/$lib/$b.o" "-D__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
    objs="$objs gexw3/unix/$lib/$b.o"
  done
  reg="$reg $lib.so:data:__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
done
# win32u.so: its syscalls register at __wine_unix_lib_init; the names it
# shares with wineserver and ntdll.so renamed apart
w32defs="-D__wine_unix_lib_init=win32u___wine_unix_lib_init"
for n in alloc_user_handle destroy_thread_windows free_user_handle get_virtual_screen_rect get_window_thread init_startup_info is_desktop_class is_message_class is_window_visible mirror_region send_notify_message shared_session; do
  w32defs="$w32defs -D$n=win32u_$n"
done
for f in $(grep -o "LOAD_FUNCPTR(FT_[A-Za-z_]*)" "$F/dlls/win32u/freetype.c" | sort -u | sed 's/LOAD_FUNCPTR(\(.*\))/\1/'); do reg="$reg libfreetype.so.6:func:$f"; done
mkdir -p gexw3/unix/win32u
for c in $(sed -n "/^dlls\/win32u\/win32u.so:/,/^\t/p" Makefile | grep -o "dlls/win32u/[a-z_0-9/]*\.o" | sort -u); do
  rel=${c#dlls/win32u/}; o=gexw3/unix/win32u/${rel//\//_}
  compile "$F/dlls/win32u/${rel%.o}.c" "$c" "$o" "$w32defs"; objs="$objs $o"
done
reg="$reg win32u.so:func:__wine_unix_lib_init=win32u___wine_unix_lib_init"
# winewayland.so: generated protocol sources are in the build tree
mkdir -p gexw3/unix/winewayland
make -j"$JOBS" $(sed -n "/^dlls\/winewayland.drv\/winewayland.so:/,/^\t/p" Makefile | grep -o "dlls/winewayland.drv/[a-z_0-9-]*\.o" | sort -u) >&3 2>&1 || true
for c in $(sed -n "/^dlls\/winewayland.drv\/winewayland.so:/,/^\t/p" Makefile | grep -o "dlls/winewayland.drv/[a-z_0-9-]*\.o" | sort -u); do
  b=$(basename "$c" .o); src=$F/dlls/winewayland.drv/$b.c; [ -f "$src" ] || src=dlls/winewayland.drv/$b.c
  compile "$src" "$c" "gexw3/unix/winewayland/$b.o" "-D__wine_unix_call_funcs=winewayland__wine_unix_call_funcs -Dget_region_data=winewayland_get_region_data"
  objs="$objs gexw3/unix/winewayland/$b.o"
done
reg="$reg winewayland.so:data:__wine_unix_call_funcs=winewayland__wine_unix_call_funcs"
wllibs="$D/wl/install/lib/libwayland-egl.a $D/wl/install/lib/libwayland-client.a $D/wl/install/lib/libxkbcommon.a $D/wl/install/lib/libxkbregistry.a $D/wl/install/lib/libxml2.a"
mkdir -p gexw3/unix/opengl32
for c in $(sed -n "/^dlls\/opengl32\/opengl32.so:/,/^\t/p" Makefile | grep -o "dlls/opengl32/[a-z_0-9]*\.o" | sort -u); do
  b=$(basename "$c" .o); compile "$F/dlls/opengl32/$b.c" "$c" "gexw3/unix/opengl32/$b.o" "-D__wine_unix_call_funcs=opengl32__wine_unix_call_funcs"
  objs="$objs gexw3/unix/opengl32/$b.o"
done
reg="$reg opengl32.so:data:__wine_unix_call_funcs=opengl32__wine_unix_call_funcs"
for f in $(llvm-nm --defined-only --extern-only "$D/gl/gexgl.o" | awk '$2 == "T" { print $3 }'); do reg="$reg libEGL.so.1:func:$f"; done
for f in wl_display_flush wl_proxy_destroy wl_proxy_get_display wl_proxy_get_id wl_proxy_marshal wl_proxy_marshal_constructor; do reg="$reg libwayland-client.so.0:func:$f"; done
reg="$reg libwayland-client.so.0:data:wl_callback_interface"
# wineoss.so: configure leaves it out (no OSSv4 header in the sysroot): its
# objects are compiled like crypt32's, against deps/oss's sys/soundcard.h
mkdir -p gexw3/unix/wineoss
tmpl=$(make -n -W "$F/dlls/crypt32/unixlib.c" dlls/crypt32/unixlib.o 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o dlls/crypt32/unixlib.o")
for b in midipatch oss ossmidi; do
  o=gexw3/unix/wineoss/$b.o; src=$F/dlls/wineoss.drv/$b.c
  cmd=${tmpl//dlls\/crypt32\/unixlib.o/$o}; cmd=${cmd//$F\/dlls\/crypt32\/unixlib.c/$src}
  cmd=${cmd//-Idlls\/crypt32/-Idlls\/wineoss.drv}; cmd=${cmd//-I$F\/dlls\/crypt32/-I$F\/dlls\/wineoss.drv -I$D\/oss\/include}; cmd=${cmd//-D_CRYPT32_/}
  eval "nice $cmd -D__wine_unix_call_funcs=wineoss__wine_unix_call_funcs" >&3 2>&1 || { echo "wineoss $b failed" >&2; exit 1; }
  objs="$objs $o"
done
reg="$reg wineoss.so:data:__wine_unix_call_funcs=wineoss__wine_unix_call_funcs"
python3 "$T/dlreg.py" ntdll.so:func:__wine_main $reg > gexw3/reg.c
"$WCC" -O1 -Wl,--gex-guest-i386 -o "$DIST/lib/wine/i386-unix/wine" loader/main.o gexw3/reg.c $objs \
  "$D/ft/install/lib/libfreetype.a" $wllibs "$D/gl/gexgl.o" >&3 2>&1 || { tail -20 "$L/link-loader.log"; exit 1; }
step "done: $DIST/lib/wine/i386-unix/wine ($(du -h "$DIST/lib/wine/i386-unix/wine" | cut -f1))"
