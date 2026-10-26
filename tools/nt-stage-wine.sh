#!/usr/bin/env bash
# Stage Wine's PE DLLs and NLS files into a gex Windows prefix, which 32-bit
# Wine's dist is staged from (tools/wine-wasm/stage-native.sh).
#
#   tools/nt-stage-wine.sh [PREFIX]          stage (default ~/.cache/gex/wine-11.15)
#   tools/nt-stage-wine.sh --manifest        regenerate the manifest from the host
#
# The prefix gets
#   drive_c/windows/system32/  <- $WINE_LIB/x86_64-windows/*  (PE32+ processes)
#   drive_c/windows/syswow64/  <- $WINE_LIB/i386-windows/*    (PE32 processes)
#   nls/                       <- $WINE_SHARE/nls/*.nls
# Nothing from x86_64-unix: gex replaces the Unix side of Wine entirely.
#
# Every file is checked against tools/nt-wine-11.15.manifest (path, size,
# SHA-256). The dist is tied to one Wine release, so a mismatch is an error.
set -euo pipefail

VERSION=11.15
HERE=$(cd "$(dirname "$0")" && pwd)
MANIFEST="$HERE/nt-wine-$VERSION.manifest"
WINE_LIB=${WINE_LIB:-/usr/lib/wine}
WINE_SHARE=${WINE_SHARE:-/usr/share/wine}

# (source dir, destination dir inside the prefix, glob)
SETS=(
    "$WINE_LIB/x86_64-windows|drive_c/windows/system32|*"
    "$WINE_LIB/i386-windows|drive_c/windows/syswow64|*"
    "$WINE_SHARE/nls|nls|*.nls"
    # The bitmap and TrueType fonts Wine installs in C:\windows\Fonts; the NT
    # layer's GDI text draws with the bitmap ones (nt/gdi/font.rs).
    "$WINE_SHARE/fonts|drive_c/windows/Fonts|*"
)

host_version() {
    local v=""
    if command -v wine >/dev/null 2>&1; then v=$(wine --version 2>/dev/null || true); fi
    echo "${v#wine-}"
}

list_files() {
    local set src dst glob f
    for set in "${SETS[@]}"; do
        IFS='|' read -r src dst glob <<<"$set"
        for f in "$src"/$glob; do
            [ -f "$f" ] || continue
            printf '%s|%s\n' "$f" "$dst/$(basename "$f")"
        done
    done
}

if [ "${1:-}" = "--manifest" ]; then
    hv=$(host_version)
    if [ "$hv" != "$VERSION" ]; then
        echo "nt-stage-wine: host Wine is '$hv', manifest is for $VERSION" >&2
        exit 1
    fi
    {
        echo "# Wine $VERSION PE files staged by tools/nt-stage-wine.sh: <sha256> <size> <path in prefix>"
        list_files | while IFS='|' read -r src rel; do
            printf '%s %s %s\n' "$(sha256sum "$src" | cut -d' ' -f1)" "$(stat -c %s "$src")" "$rel"
        done
    } >"$MANIFEST"
    echo "wrote $MANIFEST ($(grep -vc '^#' "$MANIFEST") files)"
    exit 0
fi

PREFIX=${1:-${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-$VERSION}}
[ -f "$MANIFEST" ] || { echo "nt-stage-wine: no manifest at $MANIFEST" >&2; exit 1; }

declare -A want_sha want_size
while read -r sha size rel; do
    [[ "$sha" == \#* ]] && continue
    want_sha[$rel]=$sha
    want_size[$rel]=$size
done <"$MANIFEST"

mkdir -p "$PREFIX"
n=0 bad=0
while IFS='|' read -r src rel; do
    if [ -z "${want_sha[$rel]:-}" ]; then
        echo "nt-stage-wine: $src is not in the manifest" >&2
        bad=$((bad + 1))
        continue
    fi
    dst="$PREFIX/$rel"
    if [ ! -f "$dst" ] || [ "$(stat -c %s "$dst")" != "${want_size[$rel]}" ]; then
        mkdir -p "$(dirname "$dst")"
        # A hard link when the prefix is on the same filesystem, else a copy.
        ln -f "$src" "$dst" 2>/dev/null || cp -f "$src" "$dst"
    fi
    if [ "$(sha256sum "$dst" | cut -d' ' -f1)" != "${want_sha[$rel]}" ]; then
        echo "nt-stage-wine: $rel does not match the Wine $VERSION manifest" >&2
        bad=$((bad + 1))
    fi
    unset "want_sha[$rel]"
    n=$((n + 1))
done < <(list_files)

for rel in "${!want_sha[@]}"; do
    echo "nt-stage-wine: $rel is in the manifest but not on this host" >&2
    bad=$((bad + 1))
done
if [ "$bad" -gt 0 ]; then
    echo "nt-stage-wine: $bad problem(s); is the host's Wine $VERSION?" >&2
    exit 1
fi
# The profile wineboot would make for the user "gex": programs expect its
# folders, the temporary directory above all, to exist.
for d in "AppData/Local/Temp" "AppData/LocalLow" "AppData/Roaming" Desktop Documents Downloads \
    Music Pictures Videos "Saved Games" Favorites Links Contacts Searches; do
    mkdir -p "$PREFIX/drive_c/users/gex/$d"
done
for d in Desktop Documents Downloads Music Pictures Videos; do
    mkdir -p "$PREFIX/drive_c/users/Public/$d"
done
mkdir -p "$PREFIX/drive_c/ProgramData" "$PREFIX/drive_c/Program Files" "$PREFIX/drive_c/Program Files (x86)" \
    "$PREFIX/drive_c/windows/temp"
echo "$VERSION" >"$PREFIX/.wine-version"
echo "staged $n files for Wine $VERSION in $PREFIX"
