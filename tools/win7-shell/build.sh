#!/bin/bash
# The Windows 7 shell site, every step in order (docs/win7-shell.md):
#   tools/win7-shell/build.sh ISO
# with W7_OUT, WINE_SRC, MUSL_SRC (and optionally GEX_CRT_SRC, JOBS,
# W7_USER) set as lib.sh says. Each step can also be run alone, in this
# order; a step's log is in $W7_OUT/logs. FROM=NN starts at step NN.
set -euo pipefail
T=$(cd "$(dirname "$0")" && pwd)
ISO=${1:?usage: build.sh ISO}
for s in 01-image 10-sysroot 11-wine-tools 12-deps 13-wine-wasm 15-wine-i386 14-link-loader 16-dist \
         20-runtime 30-prefix 31-stage 32-bundle 33-units 34-site; do
  [ "${s%%-*}" -ge "${FROM:-0}" ] || continue
  if [ "$s" = 01-image ]; then "$T/$s.sh" "$ISO"; else "$T/$s.sh"; fi
done
