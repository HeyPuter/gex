#!/bin/bash
# A 32-bit Wine install and its prefixes, built from a given Wine checkout, as
# wine64-dist.sh is for 64-bit Wine (`make` builds the one wine32.pins names):
#
#   tools/wine-wasm/wine32-dist.sh WINE CACHE
#
# prints WINE32_DIST=..., WINE32_PREFIX=... (Wayland), WINE32_PREFIX_NULL=... (the null
# driver), WINE32_PREFIX_BARE=... (staged, not installed: for a wineboot of
# one's own), WINE32_KEY=... and WINE32_WINE=..., to eval. It is this directory's
# Makefile with WINE_DIR=WINE and WINE32_CACHE=CACHE: every output is cached
# there by the Wine tree and gex's inputs (the sysroot's and libraries'
# pins, these tools, guest/gl), so a rerun on the same commit only checks,
# and the sysroot and libraries are shared with every other 32-bit Wine build in
# CACHE. The prefixes are made by wineboot under GEXC (default this tree's
# target/x86/release/gexc). The PE side is Wine's own DLLs (the host's
# staged x86 Wine prefix, GEX_WINE_PREFIX, with the core DLLs from WINE):
# no Windows binaries.
# Env: GEXC; WINE32_JOBS (default 12). The log of a build is CACHE/wine32-dist-KEY.log.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
W=$(cd "${1:?WINE}" && pwd); mkdir -p "${2:?CACHE}"; C=$(cd "$2" && pwd)
# a commit, not a working state: the key below is the checkout's tree
[ -z "$(git -C "$W" status --porcelain --untracked-files=no)" ] || { echo "wine32-dist: $W has uncommitted changes" >&2; exit 1; }
K=$(git -C "$W" rev-parse --short=12 HEAD)
mk() { make -s -C "$here" WINE_DIR="$W" WINE32_CACHE="$C" ${GEXC:+GEXC="$GEXC"} "$@"; }
paths=$(mk paths) || { echo "wine32-dist: make paths failed" >&2; exit 1; }
# the log by the whole key: two builds of one commit with different gex inputs may run at once
key=$(sed -n 's/^WINE32_KEY=//p' <<< "$paths"); log=$C/wine32-dist-$key.log
echo "wine32-dist: Wine $K, $key (log $log)" >&2
mk dist prefix prefix-null > "$log" 2>&1 ||
  { echo "wine32-dist: the build failed: $log" >&2; tail -5 "$log" >&2; exit 1; }
grep -E '^(WINE32_DIST|WINE32_PREFIX|WINE32_PREFIX_NULL|WINE32_PREFIX_BARE|WINE32_KEY)=' <<< "$paths"
echo "WINE32_WINE=$W"
