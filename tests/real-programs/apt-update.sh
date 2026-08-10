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

# And the same update on the host that matters, against the statically
# recompiled closure.
#
# This is the configuration the project is for: every library apt and its
# methods reach, translated ahead of time, so the run lifts almost nothing.
# It is checked here rather than by hand because "it worked when I ran it"
# is how the demo site spent a day broken.
#
# Skipped without a closure, because building one is minutes and a gigabyte.
# `CLOSURE` is a stem: `gexc build --closure ... -o $CLOSURE.wasm`.
CLOSURE=${CLOSURE:-/tmp/gex-aptu/apt}
if [ -f "$CLOSURE.wasm" ] && [ -f "$CLOSURE.pins" ]; then
    rm -rf "$ROOT/var/lib/apt/lists"
    mkdir -p "$ROOT/var/lib/apt/lists/partial"
    started=$(date +%s%N)
    GEX_PINS="$CLOSURE.pins" GEX_CLOSURE="$CLOSURE" GEX_TIME_JIT=1 \
        node --max-old-space-size=12288 shims/browser/js/run-node.mjs \
        --dir "/=$ROOT" --cwd / --argv0 /usr/bin/apt-get \
        "$CLOSURE.wasm" "$ROOT/usr/bin/apt-get" update > /tmp/gex-apt-browser.log 2>&1
    brc=$?
    bms=$(( ($(date +%s%N) - started) / 1000000 ))
    # Every process's translation count, summed. The number this exists to
    # watch: the same run without a closure is 281,254.
    blocks=$(grep -oE 'translating [0-9]+ blocks' /tmp/gex-apt-browser.log \
        | awk '{s+=$2} END {print s+0}')
    procs=$(grep -c 'translating' /tmp/gex-apt-browser.log || true)
    echo "apt-get update, node + closure: exit $brc, ${bms}ms, \
$blocks blocks over $procs processes"
    [ "$brc" = 0 ] || { echo "FAIL: node closure run exited $brc"; exit 1; }
else
    echo "apt-get update, node + closure: skipped (no closure at $CLOSURE.wasm)"
fi

# And in an actual browser.
#
# The leg above was labelled "browser" for months and runs `run-node.mjs`:
# the same shim, on Node, with the same wasm. That is a fair test of the
# translator and no test at all of the host -- and the difference turned out
# to matter more than anything the label implied. `run-node.mjs` starts the
# program as pid 1, so nothing above it ever forked; in a page the demo runs
# a shell, the shell forks, and until #101 the child died in job control
# before it reached `execve`. Every external command in a browser had been
# broken for as long as there had been one, and this suite reported "browser"
# passing throughout.
#
# So this leg drives Chromium: real Workers, real SharedArrayBuffer, a real
# interactive dash forking apt and apt forking its methods.
if [ -d web/dist ] && [ -x "${CHROME:-/usr/bin/chromium}" ]; then
    rm -rf "$ROOT/var/lib/apt/lists"
    mkdir -p "$ROOT/var/lib/apt/lists/partial"
    started=$(date +%s%N)
    node web/test/harness.mjs once --root "$ROOT" \
        'apt-get update >/dev/null 2>&1; echo "gate rc=$? lists=$(ls /var/lib/apt/lists | grep -c _Packages)"' \
        > /tmp/gex-apt-chromium.log 2>&1
    wms=$(( ($(date +%s%N) - started) / 1000000 ))
    gate=$(grep -o 'gate rc=[0-9]* lists=[0-9]*' /tmp/gex-apt-chromium.log | tail -1)
    wblocks=$(grep -oE 'translating [0-9]+ blocks' /tmp/gex-apt-chromium.log \
        | awk '{s+=$2} END {print s+0}')
    wprocs=$(grep -c 'translating' /tmp/gex-apt-chromium.log || true)
    echo "apt-get update, in a browser: $gate, ${wms}ms including browser start, \
$wblocks blocks over $wprocs processes"
    case "$gate" in
        "gate rc=0 lists=0") echo "FAIL: browser apt wrote no index"; exit 1 ;;
        "gate rc=0 lists="*) : ;;
        *) echo "FAIL: browser apt did not finish -- see /tmp/gex-apt-chromium.log"; exit 1 ;;
    esac
else
    echo "apt-get update, in a browser: skipped (no web/dist or no chromium)"
fi
echo "matches"
