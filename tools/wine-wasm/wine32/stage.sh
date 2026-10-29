#!/bin/bash
# The PE side of a 32-bit Wine dist, and the bare prefix it is staged through:
#
#   wine32/stage.sh BUILD SRC DIST BAREPFX
#
# stage-native.sh makes BAREPFX: the host's staged x86 Wine prefix
# (GEX_WINE_PREFIX, tools/nt-stage-wine.sh), its i386 DLLs rebased to
# wine32-bases.txt, with the core DLLs and WINE32_PROGRAMS replaced by wasm32
# skeletons over one module of Wine's PE side (winepe.wasm). Its syswow64
# becomes system32 (a 32-bit prefix), links into the shared x86 prefix are
# copied (wineboot writes system32), and dosdevices are made.
# DIST/lib/wine/i386-windows gets every DLL from it, DIST/share/wine the data
# wineboot and win32u read (wine.inf, nls, fonts, winmd).
# Env: GEX_WASM_SYSROOT, GEX_WINE_PREFIX.
set -eu
here=$(cd "$(dirname "$0")" && pwd); T=$(cd "$here/.." && pwd)
B=$(realpath "$1") F=$(realpath "$2") D=$(realpath -m "$3") P=$(realpath -m "$4")
x86=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
CORE="ntdll.dll kernelbase.dll kernel32.dll msvcrt.dll ucrtbase.dll vcruntime140.dll msvcp140.dll concrt140.dll win32u.dll user32.dll gdi32.dll"
# With TLS (WINE32_TLS, the Makefile's): crypt32, rsaenh and bcrypt as wasm32 too. As
# i386 the first certificate chain verification of a process took 10-30 s
# natively, past a server's wait for the request; as wasm32 under 1 s.
tlsmods=""; [ "${WINE32_TLS:-1}" != 0 ] && tlsmods=" crypt32.dll rsaenh.dll bcrypt.dll"
PROGRAMS=${WINE32_PROGRAMS:-explorer.exe winewayland.drv opengl32.dll wineboot.exe services.exe winedevice.exe plugplay.exe rundll32.exe svchost.exe rpcss.exe conhost.exe$tlsmods}
# Every module built (wine-build.sh's make is -k): one that failed stops
# the stage here, by name.
for m in $CORE $PROGRAMS; do
  n=${m%.*}
  [ -f "$B/dlls/$n/$m.so" ] || [ -f "$B/dlls/$m/$m.so" ] || [ -f "$B/programs/$n/$m.so" ] ||
    { echo "wine32/stage: $m was not built (no $m.so in $B; see $B/make.log)" >&2; exit 1; }
done
tmp=$(mktemp -d "${TMPDIR:-/tmp}/wine32-stage.XXXXXX"); trap 'rm -rf "$tmp"' EXIT
TMPDIR=$tmp "$T/stage-native.sh" "$B" "$F" "$P" $CORE $PROGRAMS
(cd "$P/drive_c/windows" && rm -rf system32 && mv syswow64 system32)
# No links into the shared x86 prefix: wineboot writes system32 through them.
find "$P" -type l -lname "$x86/*" -print0 | while IFS= read -r -d '' f; do cp --remove-destination "$(readlink -f "$f")" "$f"; done
mkdir -p "$P/dosdevices" && ln -sfn ../drive_c "$P/dosdevices/c:" && ln -sfn / "$P/dosdevices/z:"
# No user folders: the host's staged prefix's are its own runs' (a game's
# saves, the host user's profile). The install makes the page user's afresh.
rm -rf "$P/drive_c/users"
dd=$D/lib/wine/i386-windows; mkdir -p "$dd" "$D/share/wine"
for f in "$P"/drive_c/windows/system32/*; do [ -f "$f" ] && cp -p "$f" "$dd/"; done
# The in-process parts find nls/ beside the executable's directory
# (lib/wine/i386-unix/../share).
ln -sfn ../../share "$D/lib/wine/share"
rm -rf "$D/share/wine/nls" && cp -rL "$P/nls" "$D/share/wine/nls"
cp -fp "$B/loader/wine.inf" "$D/share/wine/wine.inf"
mkdir -p "$D/share/wine/fonts" && cp -f "$F"/fonts/*.ttf "$D/share/wine/fonts/"
# wine.inf's [WinmdFiles] (else setupapi's default queue callback asks for
# the media for ever).
mkdir -p "$D/share/wine/winmd" && cp -f "$B"/include/*.winmd "$D/share/wine/winmd/"
echo "wine32/stage: $D, $P"
