#!/bin/sh
# A/B two *builds of gexc* on the same programs, in CPU time.
#
#   usage: bench/ab-bin.sh <gexc-A> <gexc-B> [work-dir]
#
# `bench/suite.sh` A/Bs one binary against itself under two environments,
# which cannot express a change with no switch behind it. This takes two
# binaries instead and is otherwise the same measurement: minimum CPU time of
# the run phase, paired per round, reported by `bench/ab.mjs`.
set -eu

A="${1:?gexc for side A}"
B="${2:?gexc for side B}"
WORK="${3:-${TMPDIR:-/tmp}/gex-bench}"
HERE="$(cd "$(dirname "$0")" && pwd)"
TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"

for tool in "$A" "$B" "$CC"; do
    if [ ! -x "$tool" ]; then
        echo "skip: $tool is not there"
        exit 0
    fi
done
mkdir -p "$WORK"

for p in interp crunch; do
    [ -x "$WORK/$p" ] || "$CC" -O2 -static -o "$WORK/$p" "$HERE/programs/$p.c"
done

printf '%-9s %s\n' PROGRAM 'CPU TIME OF THE RUN PHASE'
for entry in "interp:60000000" "crunch:3"; do
    name="${entry%%:*}"
    arg="${entry#*:}"
    "$A" build --exhaustive --no-shared-memory "$WORK/$name" \
        -o "$WORK/$name.a.wasm" >/dev/null 2>&1
    "$B" build --exhaustive --no-shared-memory "$WORK/$name" \
        -o "$WORK/$name.b.wasm" >/dev/null 2>&1
    out=$(node "$HERE/ab.mjs" --reps "${GEX_BENCH_REPS:-4}" \
        "$WORK/$name.a.wasm" "$WORK/$name.b.wasm" "$WORK/$name" "$arg" 2>&1 | tail -1)
    printf '%-9s %s\n' "$name" "$out"
done
