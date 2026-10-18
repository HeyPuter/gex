#!/bin/bash
# Wine Mono into a W3 dist (w3.pins' WINE_MONO_*): the pinned tarball, kept
# once in DL and checked against its sha256 (w3/dl.sh), unpacked at
# DIST/share/wine/mono/wine-mono-VERSION, where mscoree looks through
# WINEDATADIR. Only the i386 half: W3 runs no x86-64 code.
#
#   w3/mono.sh DL DIST
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dl=$(realpath -m "$1") D=$(realpath -m "$2")
: "${WINE_MONO_VERSION:?}" "${WINE_MONO_URL:?}" "${WINE_MONO_SHA256:?}"
"$here/dl.sh" "$WINE_MONO_URL" "$WINE_MONO_SHA256" "$dl"
m=$D/share/wine/mono
rm -rf "$m"; mkdir -p "$m"
tar -C "$m" -xf "$dl/$(basename "$WINE_MONO_URL")"
v=$m/wine-mono-$WINE_MONO_VERSION
[ -f "$v/bin/libmono-2.0-x86.dll" ] || { echo "w3/mono: no $v/bin/libmono-2.0-x86.dll" >&2; exit 1; }
rm -rf "$v/lib/x86_64" "$v/bin/libmono-2.0-x86_64.dll"
