#!/bin/sh
# A/B two builds of gexc on a real program, checking the bytes as well as the
# time.
#
#   usage: bench/real.sh <gexc-A> <gexc-B>
#
# `bench/programs` are written to isolate one shape each. zstd is not: three
# compression levels take three different paths through one real library, and
# the output is a single artifact that every executed instruction contributed
# to -- so one wrong flag anywhere changes the file, and this fails rather
# than reading well.
#
# **This runs the native adapter, not the browser.** zstd starts a thread even
# at `-T1`, and a guest thread is a Worker, which needs a SharedArrayBuffer and
# the shared build; `run-node.mjs` refuses the plain one. So the numbers here
# are wasmtime's, and `tests/real-programs/BASELINE.md` is right that those are
# not a proxy for V8 -- `memory.copy` alone moves 2.4x between the two. Read it
# as a regression check with a byte-exact oracle, and take speed from
# `bench/nbench-ab.sh`, which does run in V8.
#
# Needs `tests/real-programs/zstd.sh` to have run once, for the binary and the
# corpus.
set -eu

A="${1:?gexc for side A}"
B="${2:?gexc for side B}"
WORK="${GEX_ZSTD_WORK:-${TMPDIR:-/tmp}/gex-zstd}"
REPS="${GEX_BENCH_REPS:-3}"
CORPUS="$WORK/corpus.bin"

if [ ! -x "$WORK/zstd" ] || [ ! -f "$CORPUS" ]; then
    echo "skip: no zstd in $WORK -- run tests/real-programs/zstd.sh first"
    exit 0
fi

run() {
    side="$1"; gexc="$2"; level="$3"
    # One untimed run first: the compiled module is cached, and the first run
    # of a build pays for the whole translation.
    "$gexc" run "$WORK/zstd" "-$level" -c -f "$CORPUS" >"$WORK/out.$side.$level" 2>/dev/null
    best=""
    i=0
    while [ "$i" -lt "$REPS" ]; do
        t0=$(date +%s%N)
        "$gexc" run "$WORK/zstd" "-$level" -c -f "$CORPUS" \
            >"$WORK/out.$side.$level" 2>/dev/null
        t1=$(date +%s%N)
        ms=$(( (t1 - t0) / 1000000 ))
        if [ -z "$best" ] || [ "$ms" -lt "$best" ]; then best=$ms; fi
        i=$((i + 1))
    done
    echo "$best"
}

printf '%-8s %-9s %-9s %s\n' LEVEL A B B/A
for level in 1 3 9; do
    a=$(run a "$A" "$level")
    b=$(run b "$B" "$level")
    if ! cmp -s "$WORK/out.a.$level" "$WORK/out.b.$level"; then
        echo "FAIL: level $level compressed to different bytes"
        exit 1
    fi
    awk -v l="$level" -v a="$a" -v b="$b" \
        'BEGIN { printf "%-8s %-9s %-9s %5.3f\n", "-" l, a "ms", b "ms", b / a }'
done
