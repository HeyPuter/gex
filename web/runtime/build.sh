#!/bin/sh
# The runtime API as an npm package (web/runtime/README.md): its modules built
# with vite into DIR, flat, one per entry (index.mjs, fs.mjs, windows-*.mjs, persist*.mjs, net*.mjs, audio.mjs, gl.mjs, input.mjs) and
# their shared chunks and Workers beside them, and under gex/ the files a
# system fetches: the shared-memory shim, kernel and seed of each guest
# architecture this tree has built (shims/browser/build.sh, GEX_ARCH=x86
# shims/browser/build.sh), the aarch64 export providers, the guest GL's WebGPU
# replay (gex-wgpu-replay.*), the X server
# (web/build-xwayland-wasm.sh). The modules find gex/ beside themselves;
# `new System({ baseUrl })` points elsewhere.
#
#   web/runtime/build.sh DIR
set -e
OUT=$(realpath -m "${1:?usage: web/runtime/build.sh DIR}")
RT=$(cd "$(dirname "$0")" && pwd)
WEB=$(dirname "$RT")
PUB=$WEB/public/gex
mkdir -p "$OUT"

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
  // No preload lists: they name the package's chunks, which an app's bundler
  // renames; the app's own build preloads what it keeps.
  build: { ...base.build, outDir: '$OUT', emptyOutDir: true, modulePreload: false,
           rollupOptions: {
             input: { index: '$RT/src/index.js', fs: '$RT/src/fs.js', 'windows-host': '$RT/src/windows/host.js', 'windows-single': '$RT/src/windows/single.js', 'windows-compositor': '$RT/src/windows/compositor.js', persist: '$RT/src/persist.js', 'persist-idb': '$RT/src/persist/idb.js', net: '$RT/src/net.js', 'net-wisp': '$RT/src/net/wisp.js', audio: '$RT/src/audio.js', gl: '$RT/src/gl.js', input: '$RT/src/input.js' },
             preserveEntrySignatures: 'strict',
             // Flat, so every module's import.meta.url finds gex/ at './'.
             output: { entryFileNames: '[name].mjs', chunkFileNames: '[name]-[hash].mjs', assetFileNames: '[name]-[hash][extname]' },
           } },
  worker: { ...base.worker, rollupOptions: { output: { entryFileNames: '[name]-[hash].mjs', chunkFileNames: '[name]-[hash].mjs' } } },
};
CFG
(cd "$WEB" && node_modules/.bin/vite build --config "$CONFIG")

# A Worker as vite writes it, new Worker(new URL(``+new URL(`x.mjs`,
# import.meta.url).href,``+import.meta.url)), is not one a bundler knows: the
# app's build would copy x.mjs and not the chunks it imports. Written as
# new Worker(new URL("./x.mjs", import.meta.url)), the app's bundler builds
# the Worker itself, its imports with it.
node - "$OUT" <<'JS'
const fs = require('fs'), path = require('path');
const out = process.argv[2];
const re = /new Worker\(new URL\(``\+new URL\(`([^`$]+\.m?js)`,import\.meta\.url\)\.href,``\+import\.meta\.url\),\{type:`module`(,name:`([^`$]*)`)?/g;
let n = 0;
for (const f of fs.readdirSync(out).filter((f) => f.endsWith('.mjs'))) {
  const p = path.join(out, f), s = fs.readFileSync(p, 'utf8');
  const t = s.replace(re, (m, name, named, wname) => {
    if (!fs.existsSync(path.join(out, name))) throw new Error(`${f}: a Worker of ${name}, not in the package`);
    n++;
    // and its options' type a plain literal, which vite asks for
    return `new Worker(new URL(${JSON.stringify('./' + name)},import.meta.url),{type:"module"${named ? `,name:${JSON.stringify(wname)}` : ''}`;
  });
  if (t !== s) fs.writeFileSync(p, t);
}
if (!n) throw new Error('web/runtime/build.sh: no Worker found to write for a bundler');
JS

# A release's wasm without its names and DWARF (about nine tenths of the shim
# and kernel); GEX_KEEP_NAMES=1 keeps them, for a debug package.
strip_wasm() {
    [ -z "${GEX_KEEP_NAMES:-}" ] || return 0
    for w; do
        wasm-tools strip -d '^name$' -d '^\.debug_.*' -d '^external_debug_info$' -d '^sourceMappingURL$' "$w" -o "$w.strip" && mv "$w.strip" "$w"
    done
}
[ -n "${GEX_KEEP_NAMES:-}" ] || command -v wasm-tools > /dev/null || { echo "web/runtime/build.sh: no wasm-tools to strip with (or GEX_KEEP_NAMES=1)" >&2; exit 1; }

mkdir -p "$OUT/gex"
n=0
for infix in "" .x86; do
    if [ -f "$PUB/gex_browser$infix.shared.wasm" ] && [ -f "$PUB/gex_kernel$infix.wasm" ] && [ -f "$PUB/seed$infix.shared.wasm" ]; then
        for f in gex_browser$infix.shared.wasm gex_kernel$infix.wasm seed$infix.shared.wasm; do
            cp "$PUB/$f" "$OUT/gex/$f"
            strip_wasm "$OUT/gex/$f"
        done
        n=$((n + 1))
    fi
done
[ "$n" -gt 0 ] || { echo "web/runtime/build.sh: no shim in $PUB (shims/browser/build.sh)" >&2; exit 1; }
if ls "$PUB/cgen/providers/"*.wasm > /dev/null 2>&1; then
    mkdir -p "$OUT/gex/cgen/providers"
    cp "$PUB/cgen/providers/"*.wasm "$OUT/gex/cgen/providers/"
    strip_wasm "$OUT/gex/cgen/providers/"*.wasm
fi

# The guest GL's replay of WebGPU (gl.js MesaGl: gex-wgpu-replay.js and its
# wasm, the deferred part fetched after the start).
for f in gex-wgpu-replay.js gex-wgpu-replay.wasm gex-wgpu-replay.deferred.wasm; do
    [ ! -f "$PUB/$f" ] || cp "$PUB/$f" "$OUT/gex/$f"
    case $f in *.wasm) [ ! -f "$OUT/gex/$f" ] || strip_wasm "$OUT/gex/$f" ;; esac
done
[ -f "$PUB/gex-wgpu-replay.js" ] || echo "web/runtime/build.sh: no $PUB/gex-wgpu-replay.js: MesaGl will not start" >&2

# The X server (runtime.js startXServer: gex/xwayland/, and gex/xwayland-st/
# in the single-thread build), web/build-xwayland-wasm.sh's; its compositor
# bridge zstd'd too (hostx.js moduleBytes asks for gexhostwl.wasm.zst first).
for d in xwayland xwayland-st; do
    if [ -f "$PUB/$d/Xwayland.mjs" ]; then
        cp -R "$PUB/$d" "$OUT/gex/$d"
        strip_wasm "$OUT/gex/$d/"*.wasm
        [ ! -f "$OUT/gex/$d/gexhostwl.wasm" ] || zstd -q -f -19 "$OUT/gex/$d/gexhostwl.wasm" -o "$OUT/gex/$d/gexhostwl.wasm.zst"
    elif [ "$d" = xwayland ]; then
        echo "web/runtime/build.sh: no $PUB/$d (web/build-xwayland-wasm.sh): X11 programs will not start" >&2
    fi
done

VERSION=$(cd "$WEB" && node -p "require('./package.json').version ?? '0.0.0'" 2>/dev/null || echo 0.0.0)
cat > "$OUT/package.json" <<PKG
{
  "name": "@gex/runtime",
  "version": "$VERSION",
  "description": "Linux programs in the browser: a kernel, the programs it runs, and their files",
  "license": "AGPL-3.0-only",
  "type": "module",
  "exports": {
    ".": { "types": "./index.d.ts", "default": "./index.mjs" },
    "./fs": { "types": "./fs.d.ts", "default": "./fs.mjs" },
    "./windows/host": { "types": "./windows-host.d.ts", "default": "./windows-host.mjs" },
    "./windows/single": { "types": "./windows-single.d.ts", "default": "./windows-single.mjs" },
    "./windows/compositor": { "types": "./windows-compositor.d.ts", "default": "./windows-compositor.mjs" },
    "./persist": { "types": "./persist.d.ts", "default": "./persist.mjs" },
    "./persist/idb": { "types": "./persist-idb.d.ts", "default": "./persist-idb.mjs" },
    "./net": { "types": "./net.d.ts", "default": "./net.mjs" },
    "./net/wisp": { "types": "./net-wisp.d.ts", "default": "./net-wisp.mjs" },
    "./audio": { "types": "./audio.d.ts", "default": "./audio.mjs" },
    "./gl": { "types": "./gl.d.ts", "default": "./gl.mjs" },
    "./input": { "types": "./input.d.ts", "default": "./input.mjs" },
    "./package.json": "./package.json"
  },
  "types": "./index.d.ts",
  "sideEffects": false
}
PKG
[ ! -f "$RT/README.md" ] || cp "$RT/README.md" "$OUT/README.md"
# The declarations (types/), one beside each module.
cp "$RT"/types/*.d.ts "$OUT/"
echo "web/runtime/build.sh: $OUT"
