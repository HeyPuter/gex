#!/bin/sh
# The runtime API as an npm package (web/runtime/README.md): its modules built
# with vite into DIR, flat, one per entry (index.mjs, fs.mjs, windows-*.mjs, persist*.mjs, net*.mjs, audio.mjs, gl.mjs, input.mjs) and
# their shared chunks and Workers beside them, and the files a system
# fetches, each by its own URL: the shared-memory shim, kernel and seed of each guest
# architecture this tree has built (shims/browser/build.sh, GEX_ARCH=x86
# shims/browser/build.sh), the aarch64 export providers, the guest GL's WebGPU
# replay (gex-wgpu-replay.*), the X server
# (web/build-xwayland-wasm.sh). A page imports the package, served as it is
# or through its own bundler, with nothing else to do; `new System({ baseUrl
# })` takes the files from a directory of the page's instead.
#
#   web/runtime/build.sh DIR
set -e
OUT=$(realpath -m "${1:?usage: web/runtime/build.sh DIR}")
RT=$(cd "$(dirname "$0")" && pwd)
WEB=$(dirname "$RT")
PUB=$WEB/public/gex
mkdir -p "$OUT"

STAGE=$(mktemp -d -p "$WEB" .runtime-pkg.XXXXXX)
CONFIG=$STAGE/vite.config.mjs
trap 'rm -rf "$STAGE"' EXIT

# A release's wasm without its names and DWARF (about nine tenths of the shim
# and kernel); GEX_KEEP_NAMES=1 keeps them, for a debug package.
strip_wasm() {
    [ -z "${GEX_KEEP_NAMES:-}" ] || return 0
    for w; do
        wasm-tools strip -d '^name$' -d '^\.debug_.*' -d '^external_debug_info$' -d '^sourceMappingURL$' "$w" -o "$w.strip" && mv "$w.strip" "$w"
    done
}
[ -n "${GEX_KEEP_NAMES:-}" ] || command -v wasm-tools > /dev/null || { echo "web/runtime/build.sh: no wasm-tools to strip with (or GEX_KEEP_NAMES=1)" >&2; exit 1; }

# The files a system fetches, staged under $STAGE/gex as the runtime names
# them (gex/...).
G=$STAGE/gex
mkdir -p "$G"
n=0
for infix in "" .x86; do
    if [ -f "$PUB/gex_browser$infix.shared.wasm" ] && [ -f "$PUB/gex_kernel$infix.wasm" ] && [ -f "$PUB/seed$infix.shared.wasm" ]; then
        for f in gex_browser$infix.shared.wasm gex_kernel$infix.wasm seed$infix.shared.wasm; do
            cp "$PUB/$f" "$G/$f"
            strip_wasm "$G/$f"
        done
        n=$((n + 1))
    fi
done
[ "$n" -gt 0 ] || { echo "web/runtime/build.sh: no shim in $PUB (shims/browser/build.sh)" >&2; exit 1; }
if ls "$PUB/cgen/providers/"*.wasm > /dev/null 2>&1; then
    mkdir -p "$G/cgen/providers"
    cp "$PUB/cgen/providers/"*.wasm "$G/cgen/providers/"
    strip_wasm "$G/cgen/providers/"*.wasm
fi
# The guest GL's replay of WebGPU (gl.js MesaGl: gex-wgpu-replay.js and its
# wasm, the deferred part fetched after the start).
for f in gex-wgpu-replay.js gex-wgpu-replay.wasm gex-wgpu-replay.deferred.wasm; do
    [ ! -f "$PUB/$f" ] || cp "$PUB/$f" "$G/$f"
    case $f in *.wasm) [ ! -f "$G/$f" ] || strip_wasm "$G/$f" ;; esac
done
[ -f "$PUB/gex-wgpu-replay.js" ] || echo "web/runtime/build.sh: no $PUB/gex-wgpu-replay.js: MesaGl will not start" >&2
# The X server (runtime.js startXServer: gex/xwayland/),
# web/build-xwayland-wasm.sh's. The single-thread build's (xwayland-st) is
# not the package's: a system needs SharedArrayBuffer.
if [ -f "$PUB/xwayland/Xwayland.mjs" ]; then
    mkdir -p "$G/xwayland"
    for f in Xwayland.mjs Xwayland.wasm gexhostwl.wasm gex.xkm; do cp "$PUB/xwayland/$f" "$G/xwayland/$f"; done
    strip_wasm "$G/xwayland/"*.wasm
else
    echo "web/runtime/build.sh: no $PUB/xwayland (web/build-xwayland-wasm.sh): X11 programs will not start" >&2
fi

# Their list (src/runtime-files.js), each by its own new URL(...,
# import.meta.url): the build below takes every one in, and so does a page's
# bundler after it, each naming the file where it puts it. A directory is not
# something a bundler follows.
( cd "$STAGE" && find gex -type f | LC_ALL=C sort ) | node -e '
  const lines = require("fs").readFileSync(0, "utf8").split("\n").filter(Boolean);
  const q = JSON.stringify;
  process.stdout.write("export default {\n" + lines.map((p) => `  ${q(p)}: new URL(${q("./" + p)}, import.meta.url).href,\n`).join("") + "};\n");
' > "$STAGE/runtime-files.js"

cat > "$CONFIG" <<CFG
import base from '$WEB/vite.config.js';
export default {
  ...base,
  root: '$WEB',
  // Relative, so the package works wherever it is served from.
  base: './',
  publicDir: false,
  logLevel: 'warn',
  define: { ...base.define, __GEX_RUNTIME_ASSETS__: 'null' },
  resolve: { ...base.resolve, alias: { ...base.resolve?.alias, './runtime-files.js': '$STAGE/runtime-files.js' } },
  // No preload lists: they name the package's chunks, which an app's bundler
  // renames; the app's own build preloads what it keeps. Every file a file,
  // none inlined: the runtime fetches them.
  build: { ...base.build, outDir: '$OUT', emptyOutDir: true, modulePreload: false, assetsInlineLimit: 0,
           rollupOptions: {
             input: { index: '$RT/src/index.js', fs: '$RT/src/fs.js', 'windows-host': '$RT/src/windows/host.js', 'windows-single': '$RT/src/windows/single.js', 'windows-compositor': '$RT/src/windows/compositor.js', persist: '$RT/src/persist.js', 'persist-idb': '$RT/src/persist/idb.js', net: '$RT/src/net.js', 'net-wisp': '$RT/src/net/wisp.js', audio: '$RT/src/audio.js', gl: '$RT/src/gl.js', input: '$RT/src/input.js' },
             preserveEntrySignatures: 'strict',
             output: { entryFileNames: '[name].mjs', chunkFileNames: '[name]-[hash].mjs', assetFileNames: '[name]-[hash][extname]' },
           } },
  // A Worker's build lists none of them (src/runtime-files.js as it is): the
  // system Worker takes the page's list (runtime.js pageGlobals), and a
  // second list would be a second copy of every file.
  worker: { ...base.worker, plugins: () => [{ name: 'gex-runtime-files', enforce: 'pre', load: (id) => id === '$STAGE/runtime-files.js' ? 'export default null;' : null }],
            // Each Worker one file, with no imports: a page's bundler builds a
            // Worker as a classic script by default (vite's iife), which
            // cannot import a chunk.
            rollupOptions: { output: { entryFileNames: '[name]-[hash].mjs', chunkFileNames: '[name]-[hash].mjs', assetFileNames: '[name]-[hash][extname]', codeSplitting: false } } },
};
CFG
(cd "$WEB" && node_modules/.bin/vite build --config "$CONFIG")

# A Worker or file as vite writes it, new URL(``+new URL(`x`,
# import.meta.url).href,``+import.meta.url), is not one a bundler knows: an
# app's build would leave it out. Written new URL("./x", import.meta.url), the
# app's bundler takes the file in and builds the Worker (and vite's dev
# server, which moves the package's code to node_modules/.vite/deps, points
# it back at the package); a Worker's options' type a plain literal, which
# vite asks for. A file's URL with ?no-inline, vite's word for a file kept a
# file: an app's build would make each one under 4 KB (a seed, a provider) a
# data: URL in its code; any other bundler or server reads it as a query.
node - "$OUT" <<'JS'
const fs = require('fs'), path = require('path');
const out = process.argv[2];
const here = './';
const url = /new URL\(``\+new URL\(`([^`$]+)`,import\.meta\.url\)\.href,``\+import\.meta\.url\)(,\{type:`module`(,name:`([^`$]*)`)?)?/g;
let workers = 0, files = 0;
for (const f of fs.readdirSync(out).filter((f) => f.endsWith('.mjs'))) {
  const p = path.join(out, f), s = fs.readFileSync(p, 'utf8');
  const t = s.replace(url, (m, name, opts, named, wname) => {
    if (!fs.existsSync(path.join(out, name))) throw new Error(`${f}: ${name}, not in the package`);
    if (!opts) { files++; return `new URL(${JSON.stringify(here + name + '?no-inline')},import.meta.url)`; }
    workers++;
    return `new URL(${JSON.stringify(here + name)},import.meta.url),{type:"module"${named ? `,name:${JSON.stringify(wname)}` : ''}`;
  });
  if (t !== s) fs.writeFileSync(p, t);
}
// An import() or new URL() of a URL known only at run time (a unit, the
// page's zstd module) marked as one, as the minifier left it uncommented: a
// bundler (webpack) would otherwise take in every file of the package as what
// it might name. Every one the package itself names is a literal.
let marked = 0;
for (const f of fs.readdirSync(out).filter((f) => f.endsWith('.mjs'))) {
  const p = path.join(out, f), s = fs.readFileSync(p, 'utf8');
  const t = s.replace(/\b(import|new URL)\((?![`'"]|\/\*)/g, (m) => { marked++; return `${m}/* @vite-ignore */ /* webpackIgnore: true */ `; });
  if (t !== s) fs.writeFileSync(p, t);
}
console.log(`web/runtime/build.sh: ${marked} run-time URLs marked`);
if (!workers) throw new Error('web/runtime/build.sh: no Worker found to write for a bundler');
if (!files) throw new Error('web/runtime/build.sh: none of the runtime files found to write for a bundler');
console.log(`web/runtime/build.sh: ${workers} Workers, ${files} files written for a bundler`);
JS

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
