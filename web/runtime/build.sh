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
  "repository": { "type": "git", "url": "git+https://github.com/HeyPuter/gex.git", "directory": "web/runtime" },
  "homepage": "https://github.com/HeyPuter/gex/tree/master/web/runtime#readme",
  "bugs": "https://github.com/HeyPuter/gex/issues",
  "keywords": ["linux", "wasm", "webassembly", "browser", "kernel", "x86-64", "aarch64", "wine", "wayland", "x11"],
  "files": ["*.mjs", "*.js", "*.wasm", "*.xkm", "*.d.ts", "README.md", "LICENSE", "NOTICE", "THIRD-PARTY"],
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
# gex's licence and notices, which name the third-party code it carries.
cp "$WEB/../LICENSE" "$WEB/../NOTICE" "$OUT/"

# THIRD-PARTY: the third-party code the package's files carry, each
# component's licence as its source tree has it, under a header naming the
# component and the files that hold it. A component whose files are in the
# package and whose licence file is not found fails the build. The replay's
# and the X server's licences are staged beside them by their builds
# (web/build-mesa-wasm.sh, web/build-xwayland-wasm.sh); Emscripten's from the
# emsdk that builds this, Rust's from the tree's toolchain, the crates' from
# cargo's sources.
TP=$OUT/THIRD-PARTY
in_pkg() { (cd "$OUT" && for g; do for f in $g; do if [ -f "$f" ]; then printf '%s ' "$f"; fi; done; done); }
lic() { [ -f "$1" ] || { echo "web/runtime/build.sh: no $1 ($2's licence)" >&2; exit 1; }; }
# section NAME FILES SOURCE LICENCE-FILE...
section() {
    s_name=$1 s_files=$2 s_src=$3; shift 3
    for f; do lic "$f" "$s_name"; done
    {
        printf '%s\n%s\nIn: %s\nLicence: %s\n%s\n' '================================================================================' "$s_name" "$s_files" "$s_src" '================================================================================'
        for f; do printf '\n--- %s ---\n\n' "$(basename "$f")"; cat "$f"; done
        printf '\n'
    } >> "$TP"
}
: > "$TP"
REPLAY=$(in_pkg 'gex-wgpu-replay*')
XWL=$(in_pkg 'Xwayland*')
XKM=$(in_pkg 'gex-*.xkm')
ZSTD=$(in_pkg 'zstddec*')
ZSTDW=$(in_pkg 'zstddec*.wasm')
PROV=$(in_pkg 'glibc_printf*.wasm' 'glibc_malloc*.wasm' 'crt_heap*.wasm')
if [ -n "$REPLAY" ]; then
    L=$PUB/licenses
    section "Mesa" "$REPLAY" "Mesa's docs/license.rst and licenses/" "$L/mesa/license.rst" "$L/mesa/MIT" "$L/mesa/Apache-2.0" "$L/mesa/BSL-1.0" "$L/mesa/SGI-B-2.0"
    section "Dawn and Tint" "$REPLAY" "Dawn's LICENSE" "$L/dawn/LICENSE"
    section "emdawnwebgpu" "$REPLAY" "emdawnwebgpu's webgpu_cpp/LICENSE" "$L/emdawnwebgpu/LICENSE"
fi
if [ -n "$XWL" ]; then
    L=$PUB/xwayland/licenses
    section "Xwayland (the X.Org X server)" "$XWL" "xserver's COPYING" "$L/xserver"
    section "libwayland" "$XWL" "wayland's COPYING" "$L/wayland"
    for n in pixman libxcvt libxtrans libXau libfontenc libXfont2 libxkbfile zlib; do
        section "$n" "$XWL" "$n's licence file" "$L/$n"
    done
fi
[ -z "$XKM" ] || section "xkeyboard-config" "$XKM" "the build host's xkeyboard-config copyright" "$PUB/xwayland/licenses/xkeyboard-config"
EMLIB=
if [ -n "$REPLAY$XWL$ZSTDW$PROV" ]; then
    command -v emcc > /dev/null || { echo "web/runtime/build.sh: no emcc: Emscripten's licences" >&2; exit 1; }
    EM=$(dirname "$(readlink -f "$(command -v emcc)")")
    EMLIB=$EM/system/lib
    section "Emscripten's runtime and system libraries (libc++, libc++abi, libunwind, compiler-rt)" "$REPLAY$XWL$ZSTDW" "emscripten's LICENSE and system/lib's" \
        "$EM/LICENSE" "$EMLIB/libcxx/LICENSE.TXT" "$EMLIB/libcxxabi/LICENSE.TXT" "$EMLIB/libunwind/LICENSE.TXT" "$EMLIB/compiler-rt/LICENSE.TXT"
    section "musl libc (Emscripten's libc$( [ -z "$PROV" ] || echo "; the printf providers' formatting core"))" "$REPLAY$XWL$ZSTDW$PROV" "emscripten's system/lib/libc/musl/COPYRIGHT" "$EMLIB/libc/musl/COPYRIGHT"
    lic "$EMLIB/dlmalloc.c" dlmalloc
    DL=$STAGE/dlmalloc-notice
    grep -m1 -B1 -A3 'released to the public domain' "$EMLIB/dlmalloc.c" > "$DL" || { echo "web/runtime/build.sh: no public-domain notice in $EMLIB/dlmalloc.c" >&2; exit 1; }
    section "dlmalloc (Emscripten's malloc$( [ -z "$PROV" ] || echo "; the heap providers'"))" "$REPLAY$XWL$ZSTDW$PROV" "the notice in emscripten's system/lib/dlmalloc.c" "$DL"
fi
[ -z "$ZSTD" ] || section "zstddec" "$ZSTD" "zstddec's LICENSE" "$WEB/eject-site/vendor/zstddec-LICENSE"
RUSTW=$(in_pkg 'gex_browser*.wasm' 'gex_kernel*.wasm' 'gexhostwl*.wasm')
if [ -n "$RUSTW" ]; then
    SYSROOT=$(cd "$WEB/.." && rustc --print sysroot)
    lic "$SYSROOT/share/doc/rust/COPYRIGHT-library.html" "Rust's standard library"
    RS=$STAGE/rust-COPYRIGHT-library.txt
    sed -e 's/<[^>]*>//g' -e 's/&#34;/"/g' -e "s/&#39;/'/g" -e 's/&#60;/</g' -e 's/&#62;/>/g' -e 's/&amp;/\&/g' "$SYSROOT/share/doc/rust/COPYRIGHT-library.html" | cat -s > "$RS"
    section "The Rust standard library (core, alloc, std)" "$RUSTW" "the toolchain's share/doc/rust/COPYRIGHT-library.html, as text" "$RS"
    # The crates linked into each Rust module, not the build's (proc macros
    # and what only they use).
    (cd "$WEB/.." && cargo metadata --format-version 1 --filter-platform wasm32-unknown-unknown --locked) > "$STAGE/meta.json"
    node - "$STAGE/meta.json" "$OUT" "$WEB/../third_party/wasmtime-49.0.2/LICENSE" >> "$TP" <<'JS' || exit 1
const fs = require('fs'), path = require('path');
const [meta, out, bca] = process.argv.slice(2);
const m = JSON.parse(fs.readFileSync(meta, 'utf8'));
const pkgs = new Map(m.packages.map((p) => [p.id, p]));
const nodes = new Map(m.resolve.nodes.map((n) => [n.id, n]));
const shipped = (re) => fs.readdirSync(out).filter((f) => re.test(f)).join(' ');
const roots = [['gex-browser', /^gex_browser.*\.wasm$/], ['gex-kernel-wasm', /^gex_kernel.*\.wasm$/], ['gex-hostwl', /^gexhostwl.*\.wasm$/]];
const isMacro = (p) => p.targets.some((t) => t.kind.includes('proc-macro'));
const inFiles = new Map();
for (const [name, re] of roots) {
  const files = shipped(re), root = m.packages.find((p) => p.name === name && p.source == null);
  if (!files || !root) continue;
  const seen = new Set(), todo = [root.id];
  while (todo.length) {
    const id = todo.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    if (isMacro(pkgs.get(id))) continue;
    for (const d of nodes.get(id).deps) if (d.dep_kinds.some((k) => k.kind === null)) todo.push(d.pkg);
  }
  for (const id of seen) {
    const p = pkgs.get(id);
    if (p.source == null || isMacro(p)) continue;
    inFiles.set(id, [...(inFiles.get(id) ?? []), files]);
  }
}
const bar = '='.repeat(80);
for (const [id, files] of [...inFiles].sort((a, b) => pkgs.get(a[0]).name.localeCompare(pkgs.get(b[0]).name))) {
  const p = pkgs.get(id), dir = path.dirname(p.manifest_path);
  let lics = fs.readdirSync(dir).filter((f) => /^(licen[cs]e|copying|copyright|notice)/i.test(f)).sort().map((f) => path.join(dir, f));
  let src = `the crate's ${lics.map((f) => path.basename(f)).join(', ')} (${p.license})`;
  if (!lics.length) {
    // wasm-encoder and wasmparser (the Bytecode Alliance's wasm-tools) ship
    // none: their Apache-2.0 WITH LLVM-exception, as wasmtime's tree has it.
    if (!/Apache-2\.0 WITH LLVM-exception/.test(p.license ?? '')) throw new Error(`web/runtime/build.sh: ${p.name} ${p.version}: no licence file`);
    lics = [bca];
    src = `the crate ships no licence file (${p.license}): the Bytecode Alliance's Apache-2.0 WITH LLVM-exception, as third_party/wasmtime-49.0.2/LICENSE has it`;
  }
  process.stdout.write(`${bar}\nRust crate ${p.name} ${p.version}\nIn: ${files.join(' ')}\nLicence: ${src}\n${bar}\n`);
  for (const f of lics) process.stdout.write(`\n--- ${path.basename(f)} ---\n\n${fs.readFileSync(f, 'utf8')}\n`);
  process.stdout.write('\n');
}
JS
fi
[ -s "$TP" ] || rm -f "$TP"
# The declarations (types/), one beside each module.
cp "$RT"/types/*.d.ts "$OUT/"
echo "web/runtime/build.sh: $OUT"
