#!/bin/bash
# on edger under bench.sh: every variant, 5 rounds in rotating order, one core
cd "$(dirname "$0")"
C=${BENCH_CORES%%,*}
V=(${VARIANTS:-static pie pie-dv pie-opt split lib lp64 asyncify-ind asyncify-all})
for r in 1 2 3 4 5; do
  for i in "${!V[@]}"; do v=${V[$(( (i + r) % ${#V[@]} ))]}; taskset -c $C node harness.mjs $v; done
done > results.jsonl
node -e '
const L=require("fs").readFileSync("results.jsonl","utf8").trim().split("\n").map(JSON.parse);
const by={};for(const x of L)(by[x.variant]??=[]).push(x);
const med=a=>{a=[...a].sort((x,y)=>x-y);return a[a.length>>1]};
for(const [v,xs] of Object.entries(by)){console.log([v.padEnd(13),...["zlib","zstd","trees","strings","compileMs"].map(k=>(k+"="+med(xs.map(x=>x[k]))).padEnd(16)),"bytes="+xs[0].bytes].join(" "))}'
