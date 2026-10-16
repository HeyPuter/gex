#!/bin/bash
# sockpass.c in the page (web/nt.html, x86 shim; MP and SP): TCP sockets
# passed over SCM_RIGHTS and inherited across fork between processes that
# each have their own memory, and so carried by name (shims/browser
# carry_name, "inet:"). Its remote cases go through the page's Wisp network:
# by default a local Wisp server (web/node_modules' wisp-js, private
# addresses allowed) to a local HTTP server on this host's first non-loopback
# address; with WISP=URL, that proxy and HOST:PORT (default example.com 80).
#
#   tests/wasm32/sockpass-page.sh    (from a worktree with web/node_modules,
#                                     the x86 shims in web/public/gex and
#                                     GEX_WASM_SYSROOT set)
# Env: MODES ("mp sp"), WISP, HOST, PORT, TMO (s, default 900).
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
: "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT (tools/wine-wasm/build-sysroot.sh)}"
WORK=$(mktemp -d)
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$WORK" "$ROOT/web/public/gex/nt/sockpass-$$.tar"; }
trap cleanup EXIT
mkdir -p "$WORK/b/t" "$ROOT/web/public/gex/nt"
"$ROOT/tools/wine-wasm/wcc" -O2 -o "$WORK/b/t/sockpass" "$ROOT/tests/wasm32/sockpass.c" || exit 2
rm -f "$WORK/b/t/sockpass.wasm"
tar -C "$WORK/b" -cf "$ROOT/web/public/gex/nt/sockpass-$$.tar" t
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
if [ -n "${WISP:-}" ]; then
    H=${HOST:-example.com} P=${PORT:-80}
else
    # Not 127.0.0.1: the guest's loopback is its own, not the host's.
    H=$(hostname -I | tr ' ' '\n' | grep -v '^127\.\|:' | head -1); P=$(free_port); WP=$(free_port)
    [ -n "$H" ] || { echo "sockpass-page: no non-loopback address for the local server" >&2; exit 2; }
    mkdir -p "$WORK/www"; head -c 20000 /dev/urandom | base64 > "$WORK/www/index.html"
    (cd "$WORK/www" && exec python3 -m http.server -b "$H" "$P") > "$WORK/http.log" 2>&1 & pids+=($!)
    node "$ROOT/web/node_modules/@mercuryworkshop/wisp-js/src/bin/server_cli.mjs" -H 127.0.0.1 -P "$WP" -L WARN \
        -O '{"allow_private_ips":true,"allow_loopback_ips":true}' > "$WORK/wisp.log" 2>&1 & pids+=($!)
    WISP=ws://127.0.0.1:$WP/
    sleep 1
fi
export TMPDIR=$WORK GEX_NT_CHROME_PORT=$(free_port) GEX_NT_VITE_PORT=$(free_port) GEX_WEBGPU_SOFTWARE=${GEX_WEBGPU_SOFTWARE:-1}
export SPTAR=/gex/nt/sockpass-$$.tar SPARGV="[\"$H\",\"$P\"]" SPQ="&wisp=$WISP"
timeout -k 10 "${TMO:-900}" node "$ROOT/tests/wasm32/sockpass-page.mjs"
rc=$?
for p in $(ps -eo pid=,args= | awk '/remote-debugging-port='"$GEX_NT_CHROME_PORT"'([^0-9]|$)/ && !/awk/ {print $1}'); do kill "$p" 2>/dev/null; done
exit $rc
