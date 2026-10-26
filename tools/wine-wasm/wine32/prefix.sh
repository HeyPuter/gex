#!/bin/bash
# 32-bit Wine prefixes (docs/wine-native.md §3.17):
#
#   wine32/prefix.sh install BAREPFX DIST GEXC OUT
#       Wine's full install into a copy of the bare staged prefix: under GEXC,
#       a console launcher (tests/nt/bin/x86/childcon.exe) runs
#       `wineboot.exe -u`, whose children run wine.inf's PreInstall and
#       DefaultInstall (Mono and Gecko off, as in any unattended prefix);
#       then Wine's reg.exe turns winedbg's crash dialog off, so a crash a
#       debugger attaches to ends with its text report rather than a dialog
#       that waits for a click. Fails unless the registry has the crypto
#       providers and that setting, and wineboot exited 0.
#   wine32/prefix.sh ready INSTALLED DIST GRAPHICS OUT
#       a ready-to-run prefix from the installed one (page-prefix.sh): real
#       files, the 32-bit Wine session (wine32-session.reg), the install marked current
#       for DIST's wine.inf (no update at the first start), and GRAPHICS
#       (wayland: games; null: tests without a compositor) with
#       EmulateModeset (games set display modes; Wayland cannot).
set -eu
here=$(cd "$(dirname "$0")" && pwd); T=$(cd "$here/.." && pwd); R=$(cd "$T/../.." && pwd)
case $1 in
install)
  bare=$(realpath "$2") dist=$(realpath "$3") gexc=$(realpath "$4") out=$(realpath -m "$5")
  rm -rf "$out.tmp" && cp -a "$bare" "$out.tmp"
  printf 'WINE REGISTRY Version 2\n;; All keys relative to \\\\User\\\\S-1-5-21-0-0-0-1000\n\n#arch=win32\n\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="null"\n\n[Software\\\\Wine\\\\X11 Driver] 1791289000\n"EmulateModeset"="Y"\n' > "$out.tmp/user.reg"
  stat -c %Y "$dist/share/wine/wine.inf" > "$out.tmp/.update-timestamp"
  cp "$R/tests/nt/bin/x86/childcon.exe" "$out.tmp/drive_c/launch.exe"
  log=$out.log
  (cd "$out.tmp/drive_c" && timeout -k 10 "${WINE32_INSTALL_TIMEOUT:-3600}" "$gexc" run --env WINEPREFIX="$out.tmp" \
    --env WINEDEBUG=+wineboot --env WINEDLLOVERRIDES=mscoree,mshtml= \
    "$dist/lib/wine/i386-unix/wine" ./launch.exe 'C:\windows\system32\wineboot.exe -u') > "$log" 2>&1 || true
  n=$(grep -c 'Cryptography\\\\Defaults\\\\Provider' "$out.tmp/system.reg" 2>/dev/null || true)
  e=$(grep -a '^child exit' "$log" | tr -d '\r' | tail -1)
  (cd "$out.tmp/drive_c" && timeout -k 10 600 "$gexc" run --env WINEPREFIX="$out.tmp" --env WINEDEBUG=-all \
    "$dist/lib/wine/i386-unix/wine" reg add 'HKCU\Software\Wine\WineDbg' /v ShowCrashDialog /t REG_DWORD /d 0 /f) >> "$log" 2>&1 || true
  grep -q '^"ShowCrashDialog"=dword:00000000' "$out.tmp/user.reg" || e="$e, no ShowCrashDialog"
  if [ "${n:-0}" -gt 0 ] && [ "$e" = "child exit 0" ]; then
    rm -f "$out.tmp/drive_c/launch.exe"; rm -rf "$out" && mv "$out.tmp" "$out"
    echo "wine32/prefix: installed $out ($n crypto provider keys)"
  else
    echo "wine32/prefix: install failed ($n crypto provider keys, ${e:-no exit}); log $log, prefix $out.tmp" >&2; exit 1
  fi
  ;;
ready)
  inst=$(realpath "$2") dist=$(realpath "$3") g=$4 out=$(realpath -m "$5")
  GRAPHICS=$g "$T/page-prefix.sh" "$inst" "$dist" "$out.tmp" > /dev/null
  grep -q '^"EmulateModeset"=' "$out.tmp/user.reg" ||
    printf '\n[Software\\\\Wine\\\\X11 Driver] 1791289000\n"EmulateModeset"="Y"\n' >> "$out.tmp/user.reg"
  rm -rf "$out" && mv "$out.tmp" "$out"
  echo "wine32/prefix: $out ($g)"
  ;;
*) echo "usage: wine32/prefix.sh install|ready ..." >&2; exit 2 ;;
esac
