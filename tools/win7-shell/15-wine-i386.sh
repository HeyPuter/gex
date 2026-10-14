#!/bin/bash
# 15: the fork's i386 build (mingw PE DLLs and programs), installed into
# $W7_OUT/wine-i386-install. Every PE module of the dist that is not in the
# native wasm32 module comes from here (site-7h took most of them from a
# Wine 11.15 package, and the fork's for the few it changes).
. "$(dirname "$0")/lib.sh"; need WINE_SRC
WINE_SRC=$(cd "$WINE_SRC" && pwd); B=$W7_OUT/wine-i386; I=$W7_OUT/wine-i386-install; L=$W7_OUT/logs
step "i386 tree from $WINE_SRC ($(git -C "$WINE_SRC" rev-parse --short HEAD))"
mkdir -p "$B"; cd "$B"
[ -f Makefile ] || "$WINE_SRC/configure" --enable-archs=i386 --without-freetype --without-wayland --without-opengl \
  ${WINE_OPTS/--enable-archs= /} > "$L/wine-i386-configure.log" 2>&1 || { tail -20 "$L/wine-i386-configure.log"; exit 1; }
nice make -j"$JOBS" > "$L/wine-i386-make.log" 2>&1 || { grep -m20 -B2 -A2 "error" "$L/wine-i386-make.log"; exit 1; }
rm -rf "$I"; make install DESTDIR="$I" > "$L/wine-i386-install.log" 2>&1 || { tail -20 "$L/wine-i386-install.log"; exit 1; }
ls "$I/usr/local/lib/wine/i386-windows/kernel32.dll" "$I/usr/local/share/wine/wine.inf" > /dev/null
step "done: $I ($(ls "$I/usr/local/lib/wine/i386-windows" | wc -l) i386 modules)"
