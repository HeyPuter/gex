#!/bin/bash
# tests/nt under --cgen with Wine's own DLLs as units too, built under the
# current unit key: the gate leg that exercises prefix units (tests/nt/run.sh
# --cgen alone builds only the test programs' units; a prefix unit under a
# stale key is simply not found, and that DLL runs on tier-1).
#
#   tests/nt/cgen-prefix.sh GEXC [SET]
#
# GEXC: an x86 gexc. SET: a file of DLL names (default: the core of
# tools/nt-cgen-core.txt that tests/nt maps, below). Works on a private
# copy of the staged prefix (hard links, so cheap; `cgen-prefix --set`
# rewrites its gex-layout to the set) and a private unit cache, both under
# target/nt-gate/, so neither the user's prefix nor another run's cache is
# touched. Units are cached across runs there (by key), so a rerun with an
# unchanged gex-cgen builds nothing. Env: GEX_WINE_PREFIX (the staged
# prefix), JOBS (clang jobs, default 4), and run.sh's ONLY/ARCHES.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:?usage: tests/nt/cgen-prefix.sh GEXC [SET]}")
src=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
[ -f "$src/.wine-version" ] || { echo "no staged Wine prefix at $src" >&2; exit 2; }
work=$root/target/nt-gate
prefix=$work/prefix
mkdir -p "$work"
set_file=${2:-$work/set.txt}
if [ -z "${2:-}" ]; then
    printf '%s\n' ntdll.dll kernelbase.dll kernel32.dll ucrtbase.dll msvcrt.dll msvcr120.dll > "$set_file"
fi
# A fresh link copy each run: the staged prefix may have been restaged.
rm -rf "$prefix"
cp -al "$src" "$prefix"
rm -f "$prefix/gex-layout"
export GEX_CGEN_CACHE=$work/cgen
t0=$(date +%s)
"$gexc" cgen-prefix "$prefix" --set "$set_file" --jobs "${JOBS:-4}" 2>&1 | grep -E '^cgen-prefix: [0-9]+ units|rror' || true
echo "cgen-prefix.sh: prefix units ready in $(( $(date +%s) - t0 ))s"
GEX_WINE_PREFIX=$prefix GEXC_RUN_FLAGS=--cgen "$here/run.sh" "$gexc"
