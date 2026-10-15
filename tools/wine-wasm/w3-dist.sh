#!/bin/bash
# A W3 install and its prefixes, built from a given Wine checkout (the gate's
# pinned one, as w64-dist.sh does for W64):
#
#   tools/wine-wasm/w3-dist.sh WINE CACHE
#
# prints W3_DIST=..., W3_PREFIX=... (Wayland), W3_PREFIX_NULL=... (the null
# driver), W3_PREFIX_BARE=... (staged, not installed: for a wineboot of
# one's own), W3_KEY=... and W3_WINE=..., to eval. It is this directory's
# Makefile with WINE_DIR=WINE and W3_CACHE=CACHE: every output is cached
# there by the Wine tree and gex's inputs (the sysroot's and libraries'
# pins, these tools, guest/gl), so a rerun on the same commit only checks,
# and the sysroot and libraries are shared with every other W3 build in
# CACHE. The prefixes are made by wineboot under GEXC (default this tree's
# target/x86/release/gexc). The PE side is Wine's own DLLs (the host's
# staged x86 Wine prefix, GEX_WINE_PREFIX, with the core DLLs from WINE):
# no Windows binaries.
# Env: GEXC; W3_JOBS (default 12). The log of a build is CACHE/w3-dist-KEY.log.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
W=$(cd "${1:?WINE}" && pwd); mkdir -p "${2:?CACHE}"; C=$(cd "$2" && pwd)
# a commit, not a working state: the key below is the checkout's tree
[ -z "$(git -C "$W" status --porcelain --untracked-files=no)" ] || { echo "w3-dist: $W has uncommitted changes" >&2; exit 1; }
K=$(git -C "$W" rev-parse --short=12 HEAD)
mk() { make -s -C "$here" WINE_DIR="$W" W3_CACHE="$C" ${GEXC:+GEXC="$GEXC"} "$@"; }
paths=$(mk paths) || { echo "w3-dist: make paths failed" >&2; exit 1; }
# the log by the whole key: two builds of one commit with different gex inputs may run at once
key=$(sed -n 's/^W3_KEY=//p' <<< "$paths"); log=$C/w3-dist-$key.log
echo "w3-dist: Wine $K, $key (log $log)" >&2
mk dist prefix prefix-null > "$log" 2>&1 ||
  { echo "w3-dist: the build failed: $log" >&2; tail -5 "$log" >&2; exit 1; }
grep -E '^(W3_DIST|W3_PREFIX|W3_PREFIX_NULL|W3_PREFIX_BARE|W3_KEY)=' <<< "$paths"
echo "W3_WINE=$W"
