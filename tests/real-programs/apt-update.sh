#!/bin/sh
# `apt-get update` against a local repository, end to end.
#
# The program this project keeps measuring itself against, in a form that
# needs no network and no proxy: a flat `file:` repository with a Release, an
# InRelease and a 3,000-package index. apt still forks its `file` method,
# fetches four URIs, verifies, writes the lists and parses them, which is
# every part of an update except the socket.
#
# Wants a Debian arm64 rootfs in $ROOT -- a *complete* one. apt is made of
# helpers, and a rootfs assembled by hand from one program's DT_NEEDED
# closure is missing the ones it forks: `libseccomp` for the methods, and
# whatever `/proc/self/fd` scanning and privilege dropping reach for. Half a
# day went into chasing crashes that were nothing but that.
set -e
ROOT=${ROOT:-/tmp/apt-full}
GEXC=${GEXC:-./target/release/gexc}
[ -x "$ROOT/usr/bin/apt-get" ] || { echo "skip: no apt-get in $ROOT"; exit 0; }

rm -rf "$ROOT/var/lib/apt/lists"
mkdir -p "$ROOT/var/lib/apt/lists/partial"

started=$(date +%s%N)
"$GEXC" run --sysroot "$ROOT" --uid 0 "$ROOT/usr/bin/apt-get" update
rc=$?
ms=$(( ($(date +%s%N) - started) / 1000000 ))

lists=$(ls "$ROOT/var/lib/apt/lists" | grep -c '_Packages$' || true)
echo "apt-get update: exit $rc, ${ms}ms, $lists index file(s) written"
[ "$rc" = 0 ] || { echo "FAIL: non-zero exit"; exit 1; }
[ "$lists" -ge 1 ] || { echo "FAIL: no index written"; exit 1; }
echo "matches"
