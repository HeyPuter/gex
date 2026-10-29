#!/bin/sh
# The runtime API as an npm package (web/runtime/README.md): its modules built
# with vite into DIR, flat, one per entry (index.mjs, fs.mjs) and their shared
# chunks and Workers beside them, and under gex/ the files a system fetches:
# the shared-memory shim, kernel and seed of each guest architecture this tree
# has built (shims/browser/build.sh, GEX_ARCH=x86 shims/browser/build.sh), the
# aarch64 export providers, and pid 1 (gex-init, built here with gexcc). The
# modules find gex/ beside themselves; `new System({ baseUrl })` points
# elsewhere.
#
#   web/runtime/build.sh DIR
set -e
OUT=$(realpath -m "${1:?usage: web/runtime/build.sh DIR}")
RT=$(cd "$(dirname "$0")" && pwd)
WEB=$(dirname "$RT")
PUB=$WEB/public/gex
mkdir -p "$OUT"

# pid 1.
INIT=$PUB/gex-init
if [ ! -f "$INIT" ] || [ "$RT/init/gex-init.c" -nt "$INIT" ]; then
    "${GEXCC:-gexcc}" -O2 -Wall -o "$INIT" "$RT/init/gex-init.c"
fi

CONFIG=$(mktemp --suffix=.mjs -p "$WEB")
trap 'rm -f "$CONFIG"' EXIT
cat > "$CONFIG" <<CFG
import base from '$WEB/vite.config.js';
export default {
  ...base,
  root: '$WEB',
  // Relative, so the package works wherever it is served from.
  base: './',
  publicDir: false,
  logLevel: 'warn',
  define: { ...base.define, __GEX_RUNTIME_ASSETS__: JSON.stringify('./') },
  build: { ...base.build, outDir: '$OUT', emptyOutDir: true,
           rollupOptions: {
             input: { index: '$RT/src/index.js', fs: '$RT/src/fs.js' },
             preserveEntrySignatures: 'strict',
             // Flat, so every module's import.meta.url finds gex/ at './'.
             output: { entryFileNames: '[name].mjs', chunkFileNames: '[name]-[hash].mjs', assetFileNames: '[name]-[hash][extname]' },
           } },
  worker: { ...base.worker, rollupOptions: { output: { entryFileNames: '[name]-[hash].mjs', chunkFileNames: '[name]-[hash].mjs' } } },
};
CFG
(cd "$WEB" && node_modules/.bin/vite build --config "$CONFIG")

mkdir -p "$OUT/gex"
cp "$INIT" "$OUT/gex/gex-init"
n=0
for infix in "" .x86; do
    if [ -f "$PUB/gex_browser$infix.shared.wasm" ]; then
        for f in gex_browser$infix.shared.wasm gex_kernel$infix.wasm seed$infix.shared.wasm; do
            cp "$PUB/$f" "$OUT/gex/$f"
        done
        n=$((n + 1))
    fi
done
[ "$n" -gt 0 ] || { echo "web/runtime/build.sh: no shim in $PUB (shims/browser/build.sh)" >&2; exit 1; }
if ls "$PUB/cgen/providers/"*.wasm > /dev/null 2>&1; then
    mkdir -p "$OUT/gex/cgen/providers"
    cp "$PUB/cgen/providers/"*.wasm "$OUT/gex/cgen/providers/"
fi

VERSION=$(cd "$WEB" && node -p "require('./package.json').version ?? '0.0.0'" 2>/dev/null || echo 0.0.0)
cat > "$OUT/package.json" <<PKG
{
  "name": "@gex/runtime",
  "version": "$VERSION",
  "description": "Linux programs in the browser: a kernel, the programs it runs, and their files",
  "license": "AGPL-3.0-only",
  "type": "module",
  "exports": {
    ".": "./index.mjs",
    "./fs": "./fs.mjs",
    "./package.json": "./package.json"
  },
  "sideEffects": false
}
PKG
[ ! -f "$RT/README.md" ] || cp "$RT/README.md" "$OUT/README.md"
echo "web/runtime/build.sh: $OUT"
