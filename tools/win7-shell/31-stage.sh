#!/bin/bash
# 31: the shell's prefix, $W7_OUT/prefix: the base prefix (30) with
# Windows 7's shell staged from the image (01) by tools/explorer/stage.sh
# (the images in tools/explorer/native-explorer.txt with their .mui, COM
# classes and overrides; Resources, Branding, Web, Fonts, the account
# pictures; the stock programs and Start menu shortcuts, stage-stock.sh),
# with Windows' explorer.exe as the session's shell (SHELL_KIND=windows),
# and the session's winlogon (the Wine fork's programs/winlogon, built with
# MinGW by tools/explorer/build-winlogon.sh) as C:\winlogon.exe.
# python-registry (which reads the image's hives) is installed into
# $W7_OUT/pylib with pip, pinned.
. "$(dirname "$0")/lib.sh"; need WINE_SRC
E=$GEX_SRC/tools/explorer; IMG=$W7_OUT/img; BASE=$W7_OUT/base-prefix; OUT=$W7_OUT/prefix; L=$W7_OUT/logs
[ -f "$IMG/Windows/System32/config/SOFTWARE" ] || { echo "run 01-image.sh first" >&2; exit 2; }
[ -f "$BASE/system.reg" ] || { echo "run 30-prefix.sh first" >&2; exit 2; }
export REGISTRY_LIB=$W7_OUT/pylib
if [ ! -d "$REGISTRY_LIB/Registry" ]; then
  step "python-registry 1.3.1 (pip, into pylib)"
  python3 -m pip install -q --no-deps --target "$REGISTRY_LIB" \
    python-registry==1.3.1 enum-compat==0.0.3 unicodecsv==0.14.1 > "$L/pip.log" 2>&1 || { tail "$L/pip.log"; exit 1; }
fi
step "Windows 7's shell from the image"
SHELL_KIND=windows NATIVE="$(grep -v '^#' "$E/native-explorer.txt")" "$E/stage.sh" "$IMG" "$BASE" "$OUT" > "$L/stage.log" 2>&1 \
  || { tail -20 "$L/stage.log"; exit 1; }
step "winlogon.exe (MinGW)"
"$E/build-winlogon.sh" "$WINE_SRC" "$OUT/drive_c/winlogon.exe"
step "done: $OUT ($(du -sh "$OUT" | cut -f1))"
