#!/bin/bash
# 20: gex's runtime from this checkout: gexc for x86 guests (the native
# runner: wineboot's install, the units), the browser shim, kernel and seed
# for x86 (shims/browser/build.sh with GEX_ARCH=x86, into web/public/gex),
# and the page's node modules (web/, npm ci).
# Output: $GEX_SRC/target/x86/release/gexc, $GEX_SRC/web/public/gex/*.x86*.wasm.
. "$(dirname "$0")/lib.sh"; L=$W7_OUT/logs
cd "$GEX_SRC"
step "gexc (x86)"
cargo build --release -p gexc --features x86 --target-dir target/x86 > "$L/gexc.log" 2>&1 || { grep -m20 -A5 "^error" "$L/gexc.log"; exit 1; }
step "the browser shim, kernel and seed (x86)"
GEX_ARCH=x86 shims/browser/build.sh > "$L/shim.log" 2>&1 || { tail -20 "$L/shim.log"; exit 1; }
ls web/public/gex/gex_browser.x86.shared.wasm web/public/gex/gex_kernel.x86.wasm web/public/gex/seed.x86.shared.wasm > /dev/null
step "the page's node modules"
(cd web && npm ci --no-audit --no-fund) > "$L/npm.log" 2>&1 || { tail -20 "$L/npm.log"; exit 1; }
step "done: $GEX_SRC/target/x86/release/gexc"
