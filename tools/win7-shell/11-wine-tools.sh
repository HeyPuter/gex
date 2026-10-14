#!/bin/bash
# 11: Wine's host build tools (winebuild, widl, wrc, ...) from the fork, for
# the cross builds' --with-wine-tools. Output: $W7_OUT/wine-tools.
. "$(dirname "$0")/lib.sh"; need WINE_SRC
WINE_SRC=$(cd "$WINE_SRC" && pwd); B=$W7_OUT/wine-tools
step "host tools from $WINE_SRC ($(git -C "$WINE_SRC" rev-parse --short HEAD))"
mkdir -p "$B"; cd "$B"
[ -f Makefile ] || "$WINE_SRC/configure" $WINE_OPTS --without-mingw --without-freetype --without-wayland --without-opengl \
  > "$W7_OUT/logs/wine-tools-configure.log" 2>&1 || { tail -20 "$W7_OUT/logs/wine-tools-configure.log"; exit 1; }
nice make -j"$JOBS" __tooldeps__ > "$W7_OUT/logs/wine-tools-make.log" 2>&1 || { grep -m20 -i error "$W7_OUT/logs/wine-tools-make.log"; exit 1; }
# wrc and wmc read the locale table from the tools tree's nls/ (a build of
# all would link it there from the source)
ln -sfn "$WINE_SRC/nls/locale.nls" nls/locale.nls
ls tools/winebuild/winebuild tools/widl/widl tools/wrc/wrc nls/locale.nls > /dev/null
step "done: $B"
