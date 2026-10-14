#!/bin/sh
# An ejected program's web build (docs/eject.md, "The web build"): gex's app
# library (web/src/app.js) built with vite into DIR as app.mjs, its chunks
# under assets/, and beside it under gex/ the x86 shared-memory shim, kernel
# and seed, Mesa (the wgpu replay, web/build-mesa-wasm.sh) for programs with
# OpenGL, and under vendor/ the zstd decoder. The page is the user's
# (user/index.html imports app.mjs); there is no nt.html, and the build
# fails if anything in DIR names it.
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
           rollupOptions: {
             input: { app: '$WEB/src/app.js' },
             // A library: app.mjs keeps its exports (start, fromLaunch, setGlobals).
             preserveEntrySignatures: 'strict',
             output: { entryFileNames: (c) => (c.name === 'app' ? 'app.mjs' : 'assets/[name]-[hash].js') },
           } },
};
CFG
(cd "$WEB" && node_modules/.bin/vite build --config "$CONFIG")
mkdir -p "$OUT/gex" "$OUT/vendor"
cp "$WEB/eject-site/vendor/zstddec-stream.mjs" "$WEB/eject-site/vendor/zstddec-LICENSE" "$OUT/vendor/"
for f in gex_browser.x86.shared.wasm gex_kernel.x86.wasm seed.x86.shared.wasm; do
    cp "$WEB/public/gex/$f" "$OUT/gex/$f"
done
for f in gex-wgpu-replay.js gex-wgpu-replay.wasm; do
    if [ -f "$WEB/public/gex/$f" ]; then cp "$WEB/public/gex/$f" "$OUT/gex/$f"
    else echo "build-eject-page: no web/public/gex/$f (web/build-mesa-wasm.sh): the build has no OpenGL" >&2; fi
done
# Mesa's cold part, when the build split one off (docs/mesa-wasm-size.md).
rm -f "$OUT/gex/gex-wgpu-replay.deferred.wasm"
[ ! -f "$WEB/public/gex/gex-wgpu-replay.deferred.wasm" ] ||
    cp "$WEB/public/gex/gex-wgpu-replay.deferred.wasm" "$OUT/gex/"
[ -f "$OUT/app.mjs" ] || { echo "build-eject-page: vite wrote no app.mjs" >&2; exit 1; }
# gex's test page is not part of an ejected build.
if [ -e "$OUT/nt.html" ] || grep -rlI 'nt\.html' "$OUT" --include='*.js' --include='*.mjs' --include='*.html' --include='*.json' >/dev/null 2>&1; then
    echo "build-eject-page: the build has or names nt.html (gex's test page): $(grep -rlI 'nt\.html' "$OUT" | head -3 | tr '\n' ' ')" >&2
    exit 1
fi
echo "build-eject-page: $OUT"
