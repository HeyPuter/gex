#!/bin/bash
# node child_process in the browser runtime under node (run-node.mjs: the
# page's shim and glue), Debian arm64 node translated: the socketpair
# semantics libuv relies on (see cp.js), and nvim's system() running node.
# Correctness only; no timing.
#
#   tests/gexcc/node-child/run.sh
# Env: NODE_ROOT (a Debian arm64 tree with usr/bin/node and its libraries,
# usr/bin/nvim optional; default scratch/CC/demo/noderoot), SH (Debian
# arm64 dash; default scratch/CC/demo/shbin/usr/bin/dash), WEB (a gex web
# dir with seed.shared.wasm; default web/public/gex), TMO (seconds, 1500).
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
S=$HOME/gex-sandbox/scratch/CC/demo
D=${NODE_ROOT:-$S/noderoot}
SH=${SH:-$S/shbin/usr/bin/dash}
WEB=${WEB:-$ROOT/web/public/gex}
for f in "$D/usr/bin/node" "$SH" "$WEB/seed.shared.wasm"; do
  [ -e "$f" ] || { echo "node-child: missing $f" >&2; exit 2; }
done
WORK=$(mktemp -d); cp "$HERE/cp.js" "$WORK/"
run() {
  GEX_WEB_DIR=$WEB GEX_SHARED=1 timeout "${TMO:-1500}" node "$ROOT/shims/browser/js/run-node.mjs" \
    --dir /="$D" --dir /lib="$D/usr/lib" --file /bin/sh="$SH" --file /usr/bin/sh="$SH" --dir /tmp="$WORK" \
    --env HOME=/tmp --env PATH=/usr/bin:/bin "$@" 2>&1
}
fail=0
out=$(run --argv0 /usr/bin/node "$WEB/seed.shared.wasm" "$D/usr/bin/node" /tmp/cp.js)
echo "$out" | grep -a '^ok \|^FAIL '
echo "$out" | grep -aq '^FAIL ' && fail=1
echo "$out" | grep -aq '^END$' || { echo "FAIL cp.js did not finish"; echo "$out" | tail -5; fail=1; }
if [ -e "$D/usr/bin/nvim" ]; then
  out=$(run --argv0 /usr/bin/nvim "$WEB/seed.shared.wasm" "$D/usr/bin/nvim" --headless -c 'lua io.stdout:write("system: "..vim.fn.system({"node","-p","6*7"}))' -c q)
  if echo "$out" | grep -aq '^system: 42'; then echo "ok nvim system() runs node"; else echo "FAIL nvim system() runs node"; echo "$out" | grep -av '^\[' | tail -5; fail=1; fi
fi
rm -rf "$WORK"
[ $fail = 0 ] && echo "node-child: PASS" || echo "node-child: FAIL"
exit $fail
