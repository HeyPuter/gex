#!/bin/bash
# on edger under bench.sh: VARIANTS, ROUNDS rounds in rotating order, one
# core; each line is one harness run (median of its 5 timed runs). Report
# the median across rounds (static's zlib swings between rounds).
cd "$(dirname "$0")"
C=${BENCH_CORES%%,*}
V=(${VARIANTS:-static static-opt lp64 lp64-pre})
for r in $(seq ${ROUNDS:-3}); do
  for i in "${!V[@]}"; do v=${V[$(( (i + r) % ${#V[@]} ))]}; taskset -c $C node harness.mjs $v; done
done
