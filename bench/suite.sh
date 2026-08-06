#!/bin/sh
# A/B two ways of building the same programs, in CPU time.
#
#   usage: bench/suite.sh '<env for A>' '<env for B>' [work-dir]
#
# The two arguments are environment assignments handed to `gexc build`, so
#
#   bench/suite.sh 'GEX_REGION=1' ''
#
# builds side A with regions off and side B as configured, and reports B/A.
#
# Every number is the minimum CPU time of the run phase over several rounds,
# with each round a fresh process so neither side inherits the other's warmed
# engine. Wall time is not usable here: on a shared machine it swings 8-15%
# between rounds, which is wider than most of what is being measured. Nor is
# the load or compile phase included -- real costs, but not what a codegen
# change moves.
#
# The spread printed beside each side is how far its own rounds ranged. A
# ratio whose spread is wider than its distance from 1.0 has not said
# anything, and the honest thing is to report it that way.
#
# The programs run for seconds on purpose. A benchmark short enough that V8 is
# still promoting functions to its optimising tier measures the promotion
# rather than the code: zstd at level 9 takes 600ms here and ranges 30% across
# rounds for that reason, while at level 19 it takes five seconds and settles.
set -eu

A_ENV="${1:?environment for side A, e.g. GEX_REGION=1}"
B_ENV="${2-}"
WORK="${3:-${TMPDIR:-/tmp}/gex-bench}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
GEXC="$ROOT/target/release/gexc"
TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"

for tool in "$GEXC" "$CC"; do
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
    # shellcheck disable=SC2086
    env $A_ENV "$GEXC" build --exhaustive --no-shared-memory "$WORK/$name" \
        -o "$WORK/$name.a.wasm" >/dev/null 2>&1
    # shellcheck disable=SC2086
    env $B_ENV "$GEXC" build --exhaustive --no-shared-memory "$WORK/$name" \
        -o "$WORK/$name.b.wasm" >/dev/null 2>&1
    out=$(node "$HERE/ab.mjs" --reps "${GEX_BENCH_REPS:-4}" \
        "$WORK/$name.a.wasm" "$WORK/$name.b.wasm" "$WORK/$name" "$arg" 2>&1 | tail -1)
    printf '%-9s %s\n' "$name" "$out"
done
