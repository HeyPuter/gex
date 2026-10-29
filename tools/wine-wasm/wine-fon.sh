#!/bin/bash
# Wine's raster fonts (vgafix.fon, coure.fon, sserife.fon, vgasys.fon and the
# rest), the fixed-pitch fonts a console window picks. Wine's fonts/ build
# makes them from its .ttf with tools/sfnt2fon, which needs FreeType: the
# dists' tools trees are configured without it, so they had only the .ttf,
# bitmap-only intermediates win32u skips, and conhost fell back to a
# proportional font squeezed into its cells.
#
#   tools/wine-wasm/wine-fon.sh WINE CONFIGINC DL OUT
#
# WINE the Wine tree; CONFIGINC a configured native Wine tree's include/
# (its config.h); DL the pinned tarballs (wine32/dl.sh fetches FreeType's
# when missing); OUT gets the .fon files. A native static FreeType from
# wine32.pins' tarball and sfnt2fon over it make each .fon from its .sfd's
# `#pragma makedep font` line, as Wine's makedep does.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
[ $# = 4 ] || { echo "usage: wine-fon.sh WINE CONFIGINC DL OUT" >&2; exit 2; }
W=$(realpath "${1:?WINE}") inc=$(realpath "${2:?CONFIGINC}") dl=$(realpath -m "${3:?DL}") out=$(realpath -m "${4:?OUT}")
[ -f "$inc/config.h" ] || { echo "wine-fon: no config.h in $inc" >&2; exit 1; }
eval "$(sed -n 's/^\(FREETYPE_[A-Z0-9_]*\) *:= *\(.*\)$/\1=\2/p' "$here/wine32.pins")"
"$here/wine32/dl.sh" "$FREETYPE_URL" "$FREETYPE_SHA256" "$dl"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/wine-fon.XXXXXX"); trap 'rm -rf "$tmp"' EXIT
tar -xf "$dl/$(basename "$FREETYPE_URL")" -C "$tmp"
( cd "$tmp/freetype-$FREETYPE_VERSION" &&
  ./configure --prefix="$tmp/ft" --disable-shared --enable-static --with-zlib=no --with-bzip2=no \
    --with-png=no --with-harfbuzz=no --with-brotli=no &&
  make -j"${JOBS:-8}" && make install ) > "$tmp/freetype.log" 2>&1 ||
  { echo "wine-fon: FreeType failed:" >&2; tail -5 "$tmp/freetype.log" >&2; exit 1; }
cc -O1 -w -DHAVE_FT2BUILD_H '-DSONAME_LIBFREETYPE="libfreetype.so.6"' -I "$inc" -I "$W/include" \
  -I "$tmp/ft/include/freetype2" "$W/tools/sfnt2fon/sfnt2fon.c" -o "$tmp/sfnt2fon" "$tmp/ft/lib/libfreetype.a"
rm -rf "$out.tmp"; mkdir -p "$out.tmp"
n=0
for sfd in "$W"/fonts/*.sfd; do
  # A .sfd keeps its pragmas in a comment, lines joined by "+AAoA".
  while read -r _ _ _ fon args; do
    # shellcheck disable=SC2086
    "$tmp/sfnt2fon" -q -o "$out.tmp/$fon" "${sfd%.sfd}.ttf" $args
    n=$((n + 1))
  done < <(grep -o 'pragma makedep font [^#"]*' "$sfd" | sed 's/+AAoA$//' || true)
done
[ "$n" -gt 0 ] || { echo "wine-fon: no font pragmas in $W/fonts" >&2; exit 1; }
rm -rf "$out"; mv "$out.tmp" "$out"
