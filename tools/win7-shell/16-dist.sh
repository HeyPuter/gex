#!/bin/bash
# 16: the W3 Wine dist, $W7_OUT/dist (the bundle's /w3):
#   lib/wine/i386-unix/wine     the loader (14-link-loader.sh)
#   lib/wine/i386-windows/      the native wasm32 module winepe.wasm with the
#                               skeletons of its modules (pe-native.txt, built by
#                               tools/wine-wasm/stage-native.sh from 13's tree),
#                               and every other i386 module (15's install)
#   share/wine/                 wine.inf, nls/, fonts/, winmd/
# stage-native.sh builds on an x86 prefix; it is made here from 15's install
# (in $W7_OUT/x86-prefix) rather than from a Wine package.
. "$(dirname "$0")/lib.sh"; need WINE_SRC
F=$(cd "$WINE_SRC" && pwd); B=$W7_OUT/wine-wasm; I=$W7_OUT/wine-i386-install/usr/local; L=$W7_OUT/logs
DIST=$W7_OUT/dist; X86=$W7_OUT/x86-prefix; S=$W7_OUT/pe-stage
[ -x "$DIST/lib/wine/i386-unix/wine" ] || { echo "run 14-link-loader.sh first" >&2; exit 2; }
step "the x86 prefix stage-native.sh builds on"
rm -rf "$X86"; mkdir -p "$X86/drive_c/windows/syswow64" "$X86/drive_c/windows/system32" "$X86/nls" "$X86/drive_c/windows/Fonts"
cp -p "$I"/lib/wine/i386-windows/* "$X86/drive_c/windows/syswow64/"
cp -p "$I"/share/wine/nls/*.nls "$X86/nls/"
cp -p "$F"/fonts/*.ttf "$X86/drive_c/windows/Fonts/"
sed -n 's/^Wine version \(.*\)$/\1/p' "$F/VERSION" > "$X86/.wine-version"
step "the native wasm32 module and its skeletons"
GEX_WINE_PREFIX=$X86 WINEBUILD=$W7_OUT/wine-tools/tools/winebuild/winebuild \
  "$GEX_SRC/tools/wine-wasm/stage-native.sh" "$B" "$F" "$S" $(grep -v '^#' "$W7_TOOLS/pe-native.txt") \
  > "$L/stage-native.log" 2>&1 || { tail -20 "$L/stage-native.log"; exit 1; }
step "the dist"
dd=$DIST/lib/wine/i386-windows; rm -rf "$dd" "$DIST/share"; mkdir -p "$dd" "$DIST/share/wine/fonts" "$DIST/share/wine/winmd" "$DIST/share/wine/nls"
sys=$S/drive_c/windows/syswow64
for m in $(grep -v '^#' "$W7_TOOLS/pe-native.txt") winepe.wasm; do cp -L "$sys/$m" "$dd/$m"; done
for f in "$I"/lib/wine/i386-windows/*; do [ -e "$dd/${f##*/}" ] || cp -p "$f" "$dd/"; done
# the in-process wineserver finds nls/ beside its executable's directory
ln -sfn ../../share "$DIST/lib/wine/share"
cp -p "$B/loader/wine.inf" "$DIST/share/wine/wine.inf"
cp -p "$I"/share/wine/nls/*.nls "$DIST/share/wine/nls/"
cp -p "$F"/fonts/*.ttf "$DIST/share/wine/fonts/"
cp -p "$B"/include/*.winmd "$DIST/share/wine/winmd/"
step "done: $DIST ($(ls "$dd" | wc -l) modules)"
