#!/bin/sh
# ttypoll.c, a terminal's polling read (non-canonical, VMIN 0, VTIME 0), in
# the browser runtime (run-node, GEX_ROOT's AArch64 shim): ptys live in the
# global kernel, which a native gexc run has none of. btop froze on its first
# key without it.
#
#   test/fixtures/gexcc/dl/ttypoll.sh DIR   (gexcc and node on PATH; GEX_ROOT: a gex
#                                 checkout with page/public/gex built)
set -u
command -v gexcc > /dev/null || { echo "skip ttypoll: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
SEED=$ROOT/page/public/gex/seed.shared.wasm
[ -f "$SEED" ] || { echo "skip ttypoll: no $SEED"; exit 0; }
HERE=$(cd "$(dirname "$0")" && pwd)
W=$(mktemp -d "${1:?DIR}/ttypoll.XXXXXX")
trap 'rm -rf "$W"' EXIT
gexcc -o "$W/p" "$HERE/ttypoll.c" || { echo "FAIL ttypoll: build"; exit 1; }
want="empty: 0
typed: 1 'q'
after: 0
nonblocking: -1 EAGAIN"
fail=0
out=$(GEX_SHARED=1 timeout 60 node "$ROOT/hosts/browser/js/run-node.mjs" --file /t/p="$W/p" --argv0 /t/p "$SEED" "$W/p" 2>&1 | grep -v '^\[')
[ "$out" = "$want" ] && echo "ok ttypoll (browser runtime)" || { echo "FAIL ttypoll (browser runtime):"; printf '%s\n' "$out"; fail=1; }
exit $fail
