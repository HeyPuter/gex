#!/bin/bash
# 13: the fork's wasm32 build tree (Wine's Unix side and its PE side compiled
# for wasm32), with FreeType, the Wayland driver's libraries and gex-gl's EGL
# from 12-deps.sh. Built with make -k: modules W3 does not use may fail; the
# ones the dist needs (pe-native.txt, the loader's objects) are checked.
# Output: $W7_OUT/wine-wasm.
. "$(dirname "$0")/lib.sh"; need WINE_SRC
WINE_SRC=$(cd "$WINE_SRC" && pwd); B=$W7_OUT/wine-wasm; D=$W7_OUT/deps; L=$W7_OUT/logs
step "wasm32 tree from $WINE_SRC ($(git -C "$WINE_SRC" rev-parse --short HEAD))"
mkdir -p "$B"; cd "$B"
if [ ! -f Makefile ]; then
  PKG_CONFIG_LIBDIR=$D/wl/install/lib/pkgconfig:$D/ft/install/lib/pkgconfig:$D/gl/pkgconfig \
  "$WINE_SRC/configure" --host=wasm32-unknown-linux-musl --with-wine-tools="$W7_OUT/wine-tools" $WINE_OPTS \
    --without-mingw --without-gnutls --without-ffmpeg --without-fontconfig CC="$WCC" PKG_CONFIG=pkg-config \
    WAYLAND_SCANNER="$D/wl/host/wayland-scanner" ac_cv_lib_soname_xkbregistry=libxkbregistry.so.0 \
    ac_cv_lib_soname_freetype=libfreetype.so.6 ac_cv_lib_soname_EGL=libEGL.so.1 \
    > "$L/wine-wasm-configure.log" 2>&1 || { tail -20 "$L/wine-wasm-configure.log"; exit 1; }
fi
nice make -k -j"$JOBS" > "$L/wine-wasm-make.log" 2>&1 || step "make -k: some modules failed (see logs/wine-wasm-make.log)"
miss=0
for m in $(grep -v '^#' "$W7_TOOLS/pe-native.txt"); do
  n=${m%.*}; f=dlls/$n/$m.so; [ -d dlls/$n ] || f=dlls/$m/$n.so; [ -d dlls/$m ] && f=dlls/$m/$m.so
  [ -d programs/$n ] && f=programs/$n/$m.so
  [ -f "$f" ] || { echo "missing $f" >&2; miss=1; }
done
[ -f loader/main.o ] || { echo "missing loader/main.o" >&2; miss=1; }
[ $miss = 0 ] || exit 1
step "done: $B"
