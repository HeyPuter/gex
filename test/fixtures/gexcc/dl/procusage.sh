#!/bin/sh
# procusage.c, /proc's processor time, uptime, load and memory measured,
# in the browser runtime (run-node, GEX_ROOT's AArch64 shim and the global
# kernel, where the processes say what they used) and natively (gexcc-exec,
# whose gexc GEXCC_GEXC picks). btop showed 0% CPU and a frozen uptime
# without it.
#
#   test/fixtures/gexcc/dl/procusage.sh DIR   (gexcc and node on PATH; GEX_ROOT: a gex
#                                   checkout with page/public/gex built)
set -u
command -v gexcc > /dev/null || { echo "skip procusage: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
SEED=$ROOT/page/public/gex/seed.shared.wasm
[ -f "$SEED" ] || { echo "skip procusage: no $SEED"; exit 0; }
HERE=$(cd "$(dirname "$0")" && pwd)
W=$(mktemp -d "${1:?DIR}/procusage.XXXXXX")
trap 'rm -rf "$W"' EXIT
gexcc -o "$W/p" "$HERE/procusage.c" || { echo "FAIL procusage: build"; exit 1; }
fail=0
out=$(GEX_SHARED=1 timeout 60 node "$ROOT/hosts/browser/js/run-node.mjs" --file /t/p="$W/p" --argv0 /t/p "$SEED" "$W/p" 2>&1 | grep -v '^\[')
n=$(printf '%s\n' "$out" | grep -c ' ok')
[ "$n" = 9 ] && echo "ok procusage (browser runtime)" || { echo "FAIL procusage (browser runtime):"; printf '%s\n' "$out"; fail=1; }
out=$(cd "$W" && GEXCC_SLOTS=0 timeout 60 gexcc-exec ./p 2>&1 | grep -v '^\[')
n=$(printf '%s\n' "$out" | grep -c ' ok')
[ "$n" = 9 ] && echo "ok procusage (native)" || { echo "FAIL procusage (native):"; printf '%s\n' "$out"; fail=1; }
exit $fail
