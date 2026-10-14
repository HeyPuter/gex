#!/bin/bash
# Link a W3 wine loader from stream W's wasm32 build tree (READ ONLY: no make,
# no writes there), recompiling into $P/wobj only the sources that our Wine
# worktree (wt/wine-explorer, explorer/wine) changes against BASE. Mirrors W's
# link-w3.sh. Unix libraries the W3 tree does not link (mountmgr, which the
# shell's drive and mount point objects need) are linked in too.
# usage: link-loader.sh OUT_DIST   (env BASE, W3_TOOLS)
. ~/gex-sandbox/scratch/env.sh
set -u
W=~/gex-sandbox/scratch/W; B=$W/wine-w32-w3; WF=~/gex-sandbox/wt/wine-w3
F=~/gex-sandbox/wt/wine-explorer; P=~/gex-sandbox/scratch/X; O=$P/wobj
T=${W3_TOOLS:-~/gex-sandbox/wt/explorer/tools/wine-wasm}; BASE=${BASE:-4a16fbb8aca}
DIST=$(realpath ${1:?OUT_DIST})
export GEX_WASM_SYSROOT=$W/sr-w3/sysroot
mkdir -p $O; cd $B
changed=$(git -C $F diff --name-only $BASE -- '*.c' '*.h')
# The LPC requests (server/lpc.c) are appended to the protocol: existing
# request numbers and structures are unchanged, so only what uses the new
# requests, the handler and trace tables, or the protocol version is rebuilt.
proto_hdrs="include/wine/server_protocol.h server/request_handlers.h server/request_trace.h server/security.h"
proto_users="server/request.c server/trace.c server/thread.c dlls/ntdll/unix/server.c"
# Headers that only gain definitions (a new last NtUserCallHwnd code, a new
# window flag): only the sources using them, which changed too, need them.
additive_hdrs="include/ntuser.h dlls/win32u/ntuser_private.h dlls/ntdll/unix/unix_private.h dlls/win32u/win32u_private.h include/winuser.h dlls/user32/user_private.h include/ntgdi.h dlls/win32u/ntgdi_private.h"
# Headers whose users must be rebuilt although they did not change: the
# system call table (stubs turned into calls) and its unix-side user.
table_hdrs="dlls/win32u/win32syscalls.h"
table_users="dlls/win32u/syscall.c"
# The Wayland driver's private header: every driver source is rebuilt.
wl_hdrs="dlls/winewayland.drv/waylanddrv.h"
# The server's window station header (struct desktop): every server source
# that includes it is rebuilt.
user_hdrs="server/user.h server/file.h"
for h in $changed; do case " $proto_hdrs $additive_hdrs $table_hdrs $wl_hdrs $user_hdrs " in *" $h "*) continue;; esac
  case $h in *.h) echo "link-loader: header $h changed; extend this script" >&2; exit 1;; esac; done
changed="$changed
$(printf '%s\n' $proto_users)"
echo "$changed" | grep -qx dlls/win32u/win32syscalls.h && changed="$changed
$(printf '%s\n' $table_users)"
echo "$changed" | grep -qx dlls/winewayland.drv/waylanddrv.h && changed="$changed
$(cd $F && ls dlls/winewayland.drv/*.c)"
for h in user.h file.h; do echo "$changed" | grep -qx server/$h && changed="$changed
$(cd $F && grep -l "\"$h\"" server/*.c)"; done
# Wayland protocols the fork adds to the driver (gex_window_placement_v1):
# their code and client headers, generated here since W's tree has none.
wlp=$O/wlproto; mkdir -p $wlp
wlnew=$(cd $F/dlls/winewayland.drv && for x in *.xml; do [ -e $B/dlls/winewayland.drv/${x%.xml}-protocol.c ] || echo ${x%.xml}; done)
for p in $wlnew; do
  x=$F/dlls/winewayland.drv/$p.xml
  [ $wlp/$p-client-protocol.h -nt $x ] || $W/wl/host/wayland-scanner client-header $x $wlp/$p-client-protocol.h || exit 1
  [ $wlp/$p-protocol.c -nt $x ] || $W/wl/host/wayland-scanner private-code $x $wlp/$p-protocol.c || exit 1
done
# obj SRC_REL OBJ_IN_B EXTRA_DEFS -> path of the object to link
obj() {
  local src=$1 o=$2 defs=$3 mine
  if echo "$changed" | grep -qx "$src"; then
    mine=$O/${o//\//_}
    local hdr=$F/include/wine/server_protocol.h
    case $src in dlls/winewayland.drv/*) hdr=$F/dlls/winewayland.drv/waylanddrv.h;; server/*) hdr=$F/server/user.h;; esac
    if [ ! $mine -nt $F/$src ] || [ ! $mine -nt $F/include/wine/server_protocol.h ] || [ ! $mine -nt $hdr ]; then
      local tgt=$4
      cmd=$(make -n -W $WF/$src $tgt 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $tgt")
      cmd=${cmd//$WF/$F}
      eval "nice -n 10 $cmd $defs" -o $mine >&2 || { echo "compile $src failed" >&2; exit 1; }
    fi
    echo $mine
  else echo $B/$o; fi
}
objs=""
for c in $(grep -o "dlls/ntdll/unix/[a-z_0-9]*\.o" Makefile | sort -u | grep -v "signal_arm\|signal_i386\|signal_x86"); do
  objs="$objs $(obj ${c%.o}.c $c "" $c)"
done
for c in $(sed -n '/^server\/wineserver:/,/^\t/p' Makefile | grep -o "server/[a-z_0-9]*\.o" | sort -u); do
  b=${c#server/}; objs="$objs $(obj server/${b%.o}.c gexw3/server/$b "-include $F/server/wasm32_inproc.h" $c)"
done
# new server sources (not in W's Makefile): compiled like request.c
for new in lpc; do
  mine=$O/gexw3_server_$new.o
  if [ ! $mine -nt $F/server/$new.c ] || [ ! $mine -nt $F/include/wine/server_protocol.h ]; then
    cmd=$(make -n -W $WF/server/request.c server/request.o 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o server/request.o")
    cmd=${cmd//$WF/$F}; cmd=${cmd//server\/request./server\/$new.}
    eval "nice -n 10 $cmd -include $F/server/wasm32_inproc.h" -o $mine >&2 || { echo "compile server/$new.c failed" >&2; exit 1; }
  fi
  objs="$objs $mine"
done
reg=""
# mountmgr's unix library is not in W's link: its sources are compiled here
# (as changed ones are), so that the mount manager, which the shell's drive
# and volume code needs, loads under winedevice as nsiproxy does
changed="$changed
$(printf 'dlls/mountmgr.sys/%s.c\n' cred dbus diskarb unixlib)"
for spec in crypt32:crypt32 ws2_32:ws2_32 msv1_0:msv1_0 dnsapi:dnsapi nsiproxy.sys:nsiproxy mountmgr.sys:mountmgr; do
  dir=${spec%%:*}; lib=${spec##*:}
  for c in $(sed -n "/^dlls\/$dir\/$lib.so:/,/^\t/p" Makefile | grep -o "dlls/$dir/[a-z_0-9]*\.o" | sort -u); do
    b=$(basename $c .o); objs="$objs $(obj dlls/$dir/$b.c gexw3/unix/$lib/$b.o "-D__wine_unix_call_funcs=${lib}__wine_unix_call_funcs" $c)"
  done
  reg="$reg $lib.so:data:__wine_unix_call_funcs=${lib}__wine_unix_call_funcs"
done
w32defs="-D__wine_unix_lib_init=win32u___wine_unix_lib_init"
for n in alloc_user_handle destroy_thread_windows free_user_handle get_virtual_screen_rect get_window_thread init_startup_info is_desktop_class is_message_class is_window_visible mirror_region send_notify_message shared_session; do
  w32defs="$w32defs -D$n=win32u_$n"
done
FT=$W/ft/install
for f in $(grep -o "LOAD_FUNCPTR(FT_[A-Za-z_]*)" $F/dlls/win32u/freetype.c | sort -u | sed 's/LOAD_FUNCPTR(\(.*\))/\1/'); do reg="$reg libfreetype.so.6:func:$f"; done
for c in $(sed -n "/^dlls\/win32u\/win32u.so:/,/^\t/p" Makefile | grep -o "dlls/win32u/[a-z_0-9/]*\.o" | sort -u); do
  rel=${c#dlls/win32u/}; objs="$objs $(obj dlls/win32u/${rel%.o}.c gexw3/unix/win32u/${rel//\//_} "$w32defs" $c)"
done
reg="$reg win32u.so:func:__wine_unix_lib_init=win32u___wine_unix_lib_init"
WL=$W/wl/install
wldefs="-D__wine_unix_call_funcs=winewayland__wine_unix_call_funcs -Dget_region_data=winewayland_get_region_data -I$wlp"
for c in $(sed -n "/^dlls\/winewayland.drv\/winewayland.so:/,/^\t/p" Makefile | grep -o "dlls/winewayland.drv/[a-z_0-9-]*\.o" | sort -u); do
  b=$(basename $c .o); objs="$objs $(obj dlls/winewayland.drv/$b.c gexw3/unix/winewayland/$b.o "$wldefs" $c)"
done
# The added protocols' code, compiled as W's tree compiles its own.
for p in $wlnew; do
  mine=$O/winewayland_$p-protocol.o
  if [ ! $mine -nt $wlp/$p-protocol.c ]; then
    tgt=dlls/winewayland.drv/viewporter-protocol.o
    cmd=$(make -n -W dlls/winewayland.drv/viewporter-protocol.c $tgt 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $tgt")
    cmd=${cmd/-c -o $tgt dlls\/winewayland.drv\/viewporter-protocol.c/-c $wlp/$p-protocol.c}
    eval "nice -n 10 $cmd" -o $mine >&2 || { echo "compile $p-protocol.c failed" >&2; exit 1; }
  fi
  objs="$objs $mine"
done
reg="$reg winewayland.so:data:__wine_unix_call_funcs=winewayland__wine_unix_call_funcs"
wllibs="$WL/lib/libwayland-egl.a $WL/lib/libwayland-client.a $WL/lib/libxkbcommon.a $WL/lib/libxkbregistry.a $WL/lib/libxml2.a"
for c in $(sed -n "/^dlls\/opengl32\/opengl32.so:/,/^\t/p" Makefile | grep -o "dlls/opengl32/[a-z_0-9]*\.o" | sort -u); do
  b=$(basename $c .o); objs="$objs $(obj dlls/opengl32/$b.c gexw3/unix/opengl32/$b.o "-D__wine_unix_call_funcs=opengl32__wine_unix_call_funcs" $c)"
done
reg="$reg opengl32.so:data:__wine_unix_call_funcs=opengl32__wine_unix_call_funcs"
for f in $(llvm-nm --defined-only --extern-only $W/gl/gexgl.o | awk '$2 == "T" { print $3 }'); do reg="$reg libEGL.so.1:func:$f"; done
for f in wl_display_flush wl_proxy_destroy wl_proxy_get_display wl_proxy_get_id wl_proxy_marshal wl_proxy_marshal_constructor; do reg="$reg libwayland-client.so.0:func:$f"; done
reg="$reg libwayland-client.so.0:data:wl_callback_interface"
for b in midipatch oss ossmidi; do
  echo "$changed" | grep -qx dlls/wineoss.drv/$b.c && { echo "wineoss change: extend this script" >&2; exit 1; }
  objs="$objs $B/gexw3/unix/wineoss/$b.o"
done
reg="$reg wineoss.so:data:__wine_unix_call_funcs=wineoss__wine_unix_call_funcs"
python3 $T/dlreg.py ntdll.so:func:__wine_main $reg > $O/reg.c
cmp -s $O/reg.c $B/gexw3/reg.c || echo "link-loader: note: registry differs from W's last link (gexw3/reg.c)" >&2
mkdir -p $DIST/lib/wine/i386-unix
main=$(obj loader/main.c loader/main.o "" loader/main.o)
$T/wcc -O1 -Wl,--gex-guest-i386 -o $DIST/lib/wine/i386-unix/wine $main $O/reg.c $objs $FT/lib/libfreetype.a $wllibs $W/gl/gexgl.o
echo "linked $DIST/lib/wine/i386-unix/wine ($(echo "$changed" | wc -w) changed sources)"
