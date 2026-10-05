#!/bin/sh
# The page an ejected Windows program's web build runs in (gexc eject
# --web-page DIR): web/nt.html built with vite into DIR, and beside it under
# gex/ the x86 shared-memory shim, kernel and seed, and Mesa (the wgpu replay,
# web/build-mesa-wasm.sh) for programs with OpenGL.
#
#   web/build-eject-page.sh DIR
set -e
OUT=$(realpath -m "${1:?usage: web/build-eject-page.sh DIR}")
WEB=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
CONFIG=$(mktemp --suffix=.mjs -p "$WEB")
trap 'rm -f "$CONFIG"' EXIT
cat > "$CONFIG" <<CFG
import base from '$WEB/vite.config.js';
export default {
  ...base,
  root: '$WEB',
  publicDir: false,
  logLevel: 'warn',
  build: { ...base.build, outDir: '$OUT', emptyOutDir: true,
           rollupOptions: { input: { nt: '$WEB/nt.html' } } },
};
CFG
(cd "$WEB" && node_modules/.bin/vite build --config "$CONFIG")
mkdir -p "$OUT/gex"
for f in gex_browser.x86.shared.wasm gex_kernel.x86.wasm seed.x86.shared.wasm; do
    cp "$WEB/public/gex/$f" "$OUT/gex/$f"
done
for f in gex-wgpu-replay.js gex-wgpu-replay.wasm; do
    if [ -f "$WEB/public/gex/$f" ]; then cp "$WEB/public/gex/$f" "$OUT/gex/$f"
    else echo "build-eject-page: no web/public/gex/$f (web/build-mesa-wasm.sh): the page has no OpenGL" >&2; fi
done
echo "build-eject-page: $OUT"
