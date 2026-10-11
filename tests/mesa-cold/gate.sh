#!/bin/bash
# tests/mesa-cold/gate.sh: the gate leg for Mesa's cold part (docs/mesa-wasm-size.md), after
# `web/build-all.sh --both`. Correctness only (SwiftShader, any host); timing is the bench box's.
#   1. tests/mesa-cold/run.mjs on the mt and the st build: whole, split, a forced miss, a prefetched miss
#   2. web/test/site.mjs gears on the SP dist (Mesa in the system Worker, split, lazy)
# Prints `ok   mesa-cold gate: 3/3` when every leg passes; exits non-zero otherwise.
# MP gears needs a GPU (it fails on SwiftShader on master too): run it on the bench box.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# Chromium's singleton socket path must stay short.
export TMPDIR=$(mktemp -d /tmp/mcold.XXXX)
trap 'sleep 1; rm -rf "$TMPDIR" 2>/dev/null' EXIT
export GEX_WEBGPU_SOFTWARE=1
pass=0
for v in mt st; do
  out=$(node tests/mesa-cold/run.mjs "target/web-build/mesa-$v/build/src/gallium/targets/gex-wasm" 2>&1)
  echo "$out" | sed "s/^/[$v] /"
  echo "$out" | grep -q '^ok   mesa-cold: 4/4' && pass=$((pass + 1))
done
out=$(GEX_NO_COI=1 GEX_DIST="$ROOT/web/dist-sp" node web/test/site.mjs gears 2>&1)
echo "$out" | tail -2 | sed 's/^/[sp] /'
echo "$out" | grep -q '^ok   site-gears' && pass=$((pass + 1))
if [ $pass = 3 ]; then echo "ok   mesa-cold gate: 3/3"; else echo "FAIL mesa-cold gate: $pass/3"; exit 1; fi
