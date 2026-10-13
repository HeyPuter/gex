#!/bin/bash
# Link wineserver as its own wasm32 program, the server of door calls
# (docs/wineserver-kernel.md): every object under server/ compiled from a
# door-enabled Wine source tree (server/door.c, server/wasm32_door.h), with
# the compile commands of a configured W3 (wasm32) Wine build tree, which is
# only read (make -n), never built or written.
#
#   tools/wine-wasm/link-wineserver.sh BUILD SRC OBJ DIST
#
# BUILD  configured W3 Wine build tree (its Makefile names server/wineserver)
# SRC    Wine source with the door server (e.g. Wine branch wskern/wine)
# OBJ    directory for the server objects (reused: rebuilt when older than the
#        source or any server header)
# DIST   W3 install; writes DIST/lib/wine/i386-unix/wineserver(.wasm)
# Env: GEX_WASM_SYSROOT (the W3 sysroot), JOBS (default 6).
set -u
here=$(cd "$(dirname "$0")" && pwd)
B=$(realpath "${1:?BUILD}"); F=$(realpath "${2:?SRC}"); O=$(realpath -m "${3:?OBJ}"); DIST=$(realpath -m "${4:?DIST}")
: "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT to the W3 sysroot}"
[ -f "$F/server/door.c" ] || { echo "link-wineserver: $F has no server/door.c" >&2; exit 1; }
mkdir -p "$O"; cd "$B" || exit 1
# The source tree BUILD was configured against: make -n's paths name it.
WF=$(sed -n 's/^srcdir *= *//p' Makefile); WF=$(cd "$WF" && pwd)
compile_cmd() { # SRC_PATH OBJ_REL -> the build's compile command for it
	make -n -W "$1" "$2" 2>/dev/null | sed -e ':a' -e '/\\$/N; s/\\\n//; ta' | grep -- "-c -o $2"
}
tmpl=$(compile_cmd "$WF/server/atom.c" server/atom.o)
[ -n "$tmpl" ] || { echo "link-wineserver: no compile command for server/atom.o in $B" >&2; exit 1; }
srcs="$(sed -n '/^server\/wineserver:/,/^\t/p' Makefile | grep -o "server/[a-z_0-9]*\.o" | sort -u | sed 's#server/##; s#\.o$##') door"
objs=""; rm -f "$O/.fail"
for b in $srcs; do
	o=$O/$b.o; objs="$objs $o"; src=$F/server/$b.c
	newer=0; for h in "$F"/server/*.h; do [ "$h" -nt "$o" ] && newer=1 && break; done
	[ "$o" -nt "$src" ] && [ $newer = 0 ] && continue
	if [ $b = door ]; then cmd=${tmpl//$WF\/server\/atom.c/$src}; cmd=${cmd//-c -o server\/atom.o/-c -o $o}
	else cmd=$(compile_cmd "$WF/server/$b.c" server/$b.o); cmd=${cmd//-c -o server\/$b.o/-c -o $o}; fi
	cmd=${cmd//$WF/$F}
	( eval "$cmd -include $F/server/wasm32_door.h" ) || { echo "link-wineserver: server/$b.c failed" >&2; touch "$O/.fail"; } &
	while [ "$(jobs -r | wc -l)" -ge "${JOBS:-6}" ]; do wait -n; done
done
wait
[ -e "$O/.fail" ] && exit 1
mkdir -p "$DIST/lib/wine/i386-unix"
"$here/wcc" -O1 -o "$DIST/lib/wine/i386-unix/wineserver" $objs || { echo "link-wineserver: link failed" >&2; exit 1; }
echo "linked $DIST/lib/wine/i386-unix/wineserver"
