#!/bin/bash
# 30: the base prefix, $W7_OUT/base-prefix: Wine's own install (wineboot -u)
# run by the W3 dist under gexc, then made a page prefix
# (tools/wine-wasm/page-prefix.sh: the W3 session's registry, the install
# marked current for the dist's wine.inf, Wine's Wayland driver).
#
# The install starts from 16's staged tree (system32 = the skeletons and
# the i386 modules) and runs `wineboot.exe -u` as a child of a console
# launcher (tests/nt's childcon.exe), on Wine's null graphics driver; Wine
# Mono's and Gecko's installers are off (mscoree, mshtml), as in any
# unattended prefix. W7_USER (default claude) is the user the prefix is
# made for (C:\users\W7_USER; the site runs as the same user).
# Output: $W7_OUT/base-prefix; the install's log in logs/wineboot.log.
. "$(dirname "$0")/lib.sh"
G=$GEX_SRC/target/x86/release/gexc; DIST=$W7_OUT/dist; S=$W7_OUT/pe-stage; L=$W7_OUT/logs
I=$W7_OUT/wineboot-prefix; OUT=$W7_OUT/base-prefix; W7_USER=${W7_USER:-claude}
[ -x "$G" ] || { echo "run 20-runtime.sh first" >&2; exit 2; }
[ -d "$S/drive_c/windows/syswow64" ] || { echo "run 16-dist.sh first" >&2; exit 2; }
step "the install's starting tree"
chmod -R u+w "$I" 2>/dev/null || true; rm -rf "$I"; mkdir -p "$I/drive_c/windows"
# real files (no links into the stage or a cache): wineboot writes system32
(cd "$S" && find . -path ./dosdevices -prune -o -path ./drive_c/windows/system32 -prune -o -print0 | cpio -0pdmL --quiet "$I")
rm -rf "$I/drive_c/windows/system32"; mv "$I/drive_c/windows/syswow64" "$I/drive_c/windows/system32"
mkdir -p "$I/dosdevices"; ln -s ../drive_c "$I/dosdevices/c:"; ln -s / "$I/dosdevices/z:"
cat > "$I/user.reg" <<'REG'
WINE REGISTRY Version 2
;; All keys relative to \\User\\S-1-5-21-0-0-0-1000

#arch=win32

[Software\\Wine\\Drivers] 1791289000
"Graphics"="null"
REG
cp "$GEX_SRC/tests/nt/bin/x86/childcon.exe" "$I/drive_c/launch.exe"
step "wineboot -u (gexc; logs/wineboot.log)"
cd "$I/drive_c"
set +e
timeout "${WINEBOOT_SECS:-7200}" nice -n 10 "$G" run --env WINEPREFIX="$I" --env USER="$W7_USER" \
  --env WINEDEBUG=+wineboot --env WINEDLLOVERRIDES=mscoree,mshtml= \
  "$DIST/lib/wine/i386-unix/wine" 'C:\launch.exe' 'C:\windows\system32\wineboot.exe -u' > "$L/wineboot.log" 2>&1
rc=$?
set -e
cd "$W7_OUT"
n=$(grep -c 'Cryptography\\\\Defaults\\\\Provider' "$I/system.reg" 2>/dev/null || true)
e=$(grep -a '^child exit' "$L/wineboot.log" | tr -d '\r' || true)
[ "${n:-0}" -gt 0 ] && [ "$e" = "child exit 0" ] || { echo "wineboot failed (rc $rc, ${e:-no exit}, $n crypto provider keys): logs/wineboot.log" >&2; exit 1; }
# The install replaces modules that are loaded while it runs (kernel32,
# shell32, ...) as pending renames (PendingFileRenameOperations); a boot
# carries them out. Run one here: left in the prefix, they would replace
# the Windows files 31 stages with Wine's at the session's first boot.
step "wineboot -r: the install's pending renames (logs/wineboot-r.log)"
cd "$I/drive_c"
set +e
timeout 1800 nice -n 10 "$G" run --env WINEPREFIX="$I" --env USER="$W7_USER" \
  --env WINEDLLOVERRIDES=mscoree,mshtml= \
  "$DIST/lib/wine/i386-unix/wine" 'C:\launch.exe' 'C:\windows\system32\wineboot.exe -r' > "$L/wineboot-r.log" 2>&1
rc=$?
set -e
cd "$W7_OUT"
e=$(grep -a '^child exit' "$L/wineboot-r.log" | tr -d '\r' || true)
if [ "$e" != "child exit 0" ] || grep -q PendingFileRenameOperations "$I/system.reg" || ls "$I"/drive_c/windows/system32/dll*.tmp >/dev/null 2>&1; then
  echo "wineboot -r left pending renames (rc $rc, ${e:-no exit}): logs/wineboot-r.log" >&2; exit 1
fi
rm -f "$I/drive_c/launch.exe"
step "the page prefix"
GRAPHICS=wayland "$GEX_SRC/tools/wine-wasm/page-prefix.sh" "$I" "$DIST" "$OUT"
chmod -R u+w "$I"; rm -rf "$I"
step "done: $OUT"
