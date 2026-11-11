#!/bin/sh
# The runtime API as an npm package (page/runtime/README.md): its modules built
# with vite into DIR, flat, one per entry (index.mjs, fs.mjs, windows-*.mjs, net-wisp.mjs, audio.mjs, gl.mjs, input.mjs) and
# their shared chunks and Workers beside them, and the files a system
# fetches, each by its own URL: the shared-memory shim, kernel and seed of each guest
# architecture this tree has built (hosts/browser/build.sh, GEX_ARCH=x86
# hosts/browser/build.sh), each of those guests' export providers, the guest GL's WebGPU
# replay (gex-wgpu-replay.*), the X server
# (ports/xwayland/build.sh). A page imports the package, served as it is
# or through its own bundler, with nothing else to do; `new System({ baseUrl
# })` takes the files from a directory of the page's instead.
#
#   page/runtime/build.sh [--single] DIR
#
# Two packages, one build each. @gexemu/runtime (the default) is for a
# cross-origin isolated page: SharedArrayBuffer, a Worker per guest thread.
# @gexemu/runtime-single (--single) is for a page without SharedArrayBuffer:
# the system in one Worker, every guest thread a fiber of it (JSPI), its GL
# the single-thread Mesa (gex/st/). Each carries only its own code: the
# build's hosts/browser/js/mode.mjs says which (SP_CODE, MP_CODE), the
# bundler drops the other's branches, and a chunk nothing names any more is
# deleted.
set -e
MODE=mp
[ "${1:-}" != --single ] || { MODE=sp; shift; }
OUT=$(realpath -m "${1:?usage: page/runtime/build.sh [--single] DIR}")
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
[ -n "${GEX_KEEP_NAMES:-}" ] || command -v wasm-tools > /dev/null || { echo "page/runtime/build.sh: no wasm-tools to strip with (or GEX_KEEP_NAMES=1)" >&2; exit 1; }

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
        # Its guest's glibc providers (page/build-assets.sh): without them
        # every program of that guest runs glibc's own code, translated.
        prov=glibc_mem_a64
        [ "$infix" = .x86 ] && prov=glibc_mem
        ls "$PUB/cgen/providers/$prov"-*.wasm > /dev/null 2>&1 || {
            echo "page/runtime/build.sh: $PUB/gex_browser$infix.shared.wasm has no providers in $PUB/cgen/providers (page/build-assets.sh)" >&2; exit 1; }
    fi
done
# An x86 guest's memory64 shim and seed (runtime.js: a process with
# GEX_MEM64 or user.gex.mem64, as 64-bit Wine's programs over 4 GiB run),
# beside its wasm32 one; the kernel serves both, in either build.
if [ -f "$G/gex_browser.x86.shared.wasm" ]; then
    for f in gex_browser.x86.m64.shared.wasm seed.x86.m64.shared.wasm; do
        [ -f "$PUB/$f" ] || { echo "page/runtime/build.sh: no $PUB/$f: an x86 process with GEX_MEM64 would not start (GEX_ARCH=x86 GEX_MEM64=1 hosts/browser/build.sh)" >&2; exit 1; }
        cp "$PUB/$f" "$G/$f"
        strip_wasm "$G/$f"
    done
fi
[ "$n" -gt 0 ] || { echo "page/runtime/build.sh: no shim in $PUB (hosts/browser/build.sh)" >&2; exit 1; }
mkdir -p "$G/cgen/providers"
cp "$PUB/cgen/providers/"*.wasm "$G/cgen/providers/"
strip_wasm "$G/cgen/providers/"*.wasm
# The guest GL's replay of WebGPU (gl.js MesaGl: gex-wgpu-replay.js and its
# wasm, the deferred part fetched after the start).
# The guest GL's Mesa: the pthread build, or the single-thread one (st/).
MESA=$PUB MESAG=$G
[ $MODE = mp ] || { MESA=$PUB/st MESAG=$G/st; mkdir -p "$MESAG"; }
for f in gex-wgpu-replay.js gex-wgpu-replay.wasm gex-wgpu-replay.deferred.wasm; do
    [ ! -f "$MESA/$f" ] || cp "$MESA/$f" "$MESAG/$f"
    case $f in *.wasm) [ ! -f "$MESAG/$f" ] || strip_wasm "$MESAG/$f" ;; esac
done
[ -f "$MESA/gex-wgpu-replay.js" ] || echo "page/runtime/build.sh: no $MESA/gex-wgpu-replay.js: MesaGl will not start" >&2
# The X server (runtime.js startXServer: gex/xwayland/),
# ports/xwayland/build.sh's. The single-thread build's (xwayland-st) is
# not the package's: a system needs SharedArrayBuffer.
# The X server: the pthread build, or the single-thread one (xwayland-st/).
XW=xwayland
[ $MODE = mp ] || XW=xwayland-st
if [ -f "$PUB/$XW/Xwayland.mjs" ]; then
    mkdir -p "$G/$XW"
    for f in Xwayland.mjs Xwayland.wasm gexhostwl.wasm gex.xkm; do cp "$PUB/$XW/$f" "$G/$XW/$f"; done
    strip_wasm "$G/$XW/"*.wasm
else
    echo "page/runtime/build.sh: no $PUB/$XW (ports/xwayland/build.sh): X11 programs will not start" >&2
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

# This package's mode (hosts/browser/js/mode.mjs), in that module's place.
if [ $MODE = mp ]; then SP_CODE=false MP_CODE=true; else SP_CODE=true MP_CODE=false; fi
printf 'export const SP_CODE = %s;\nexport const MP_CODE = %s;\n' $SP_CODE $MP_CODE > "$STAGE/mode.mjs"

cat > "$CONFIG" <<CFG
import base from '$WEB/vite.config.js';
const mode = { name: 'gex-mode', enforce: 'pre', resolveId: (source) => /(^|\/)mode\.mjs\$/.test(source) ? '$STAGE/mode.mjs' : null };
export default {
  ...base,
  plugins: [...(base.plugins ?? []), mode],
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
             input: { index: '$RT/src/index.js', fs: '$RT/src/fs.js', 'windows-host': '$RT/src/windows/host.js', 'windows-single': '$RT/src/windows/single.js', 'windows-compositor': '$RT/src/windows/compositor.js', 'net-wisp': '$RT/src/net/wisp.mjs', audio: '$RT/src/audio.js', gl: '$RT/src/gl.js', input: '$RT/src/input.js' },
             preserveEntrySignatures: 'strict',
             output: { entryFileNames: '[name].mjs', chunkFileNames: '[name]-[hash].mjs', assetFileNames: '[name]-[hash][extname]' },
           } },
  // A Worker's build lists none of them (src/runtime-files.js as it is): the
  // system Worker takes the page's list (runtime.js pageGlobals), and a
  // second list would be a second copy of every file.
  // Nor any Worker runProgram's preset (run-program.js), which is the page's:
  // through it a Worker's build would take in System and its Workers again,
  // a cycle vite refuses (the single-thread system Worker takes in runtime.js).
  worker: { ...base.worker, plugins: () => [mode, { name: 'gex-runtime-files', enforce: 'pre', load: (id) => id === '$STAGE/runtime-files.js' ? 'export default null;'
              : id === '$WEB/src/run-program.js' ? 'export function runPreset() { throw new Error("runProgram runs on the page"); }' : null }],
            // Each Worker one file, with no imports: a page's bundler builds a
            // Worker as a classic script by default (vite's iife), which
            // cannot import a chunk.
            // (In place of the page's own rolldownOptions, which vite takes
            // before rollupOptions: with the page's, the Workers would go to
            // assets/ under the page's names.)
            rolldownOptions: { output: { entryFileNames: '[name]-[hash].mjs', chunkFileNames: '[name]-[hash].mjs', assetFileNames: '[name]-[hash][extname]', codeSplitting: false } } },
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
console.log(`page/runtime/build.sh: ${marked} run-time URLs marked`);
if (!workers) throw new Error('page/runtime/build.sh: no Worker found to write for a bundler');
if (!files) throw new Error('page/runtime/build.sh: none of the runtime files found to write for a bundler');
console.log(`page/runtime/build.sh: ${workers} Workers, ${files} files written for a bundler`);
JS

# A chunk or Worker of the other mode's code: built, since the bundler takes
# every Worker and import() it meets, but named by nothing once the other
# mode's branches are gone. Deleted, so the package carries none of it.
node - "$OUT" <<'JS'
const fs = require('fs'), path = require('path');
const out = process.argv[2];
const code = fs.readdirSync(out).filter((f) => /\.(mjs|js)$/.test(f));
const entries = code.filter((f) => !/-[A-Za-z0-9_-]{8}\.(mjs|js)$/.test(f));
const seen = new Set(entries), todo = [...entries];
while (todo.length) {
  const s = fs.readFileSync(path.join(out, todo.pop()), 'utf8');
  for (const f of code) if (!seen.has(f) && s.includes(f)) { seen.add(f); todo.push(f); }
}
const gone = code.filter((f) => !seen.has(f));
for (const f of gone) fs.rmSync(path.join(out, f));
console.log(`page/runtime/build.sh: ${gone.length} chunks of the other mode deleted${gone.length ? `: ${gone.join(' ')}` : ''}`);
JS

# wisp-js is in net-wisp.mjs alone: a page that imports only the runtime
# (index.mjs and every file it reaches, by import or by URL) carries none of
# it. Its marker is a string the client's own code has and gex's does not.
node - "$OUT" <<'JS'
const fs = require('fs'), path = require('path');
const out = process.argv[2];
const marker = 'wisp client warning';
const refs = (s) => [...s.matchAll(/(?:from|import\(?|new URL\()\s*(?:\/\*[^*]*\*\/\s*)*["'`]\.\/([^"'`?]+)/g)].map((m) => m[1]);
const reach = new Set(), todo = ['index.mjs'];
while (todo.length) {
  const f = todo.pop();
  if (reach.has(f) || !fs.existsSync(path.join(out, f))) continue;
  reach.add(f);
  if (/\.m?js$/.test(f)) todo.push(...refs(fs.readFileSync(path.join(out, f), 'utf8')));
}
const has = (f) => fs.readFileSync(path.join(out, f)).includes(marker);
if (!has('net-wisp.mjs')) throw new Error('page/runtime/build.sh: net-wisp.mjs has no wisp-js (the check\'s marker is stale)');
const bad = [...reach].filter(has);
if (bad.length) throw new Error(`page/runtime/build.sh: wisp-js reached from index.mjs, in ${bad.join(', ')}`);
const js = fs.readdirSync(out).filter((f) => /\.m?js$/.test(f) && f !== 'net-wisp.mjs' && has(f));
if (js.length) throw new Error(`page/runtime/build.sh: wisp-js outside net-wisp.mjs, in ${js.join(', ')}`);
console.log(`page/runtime/build.sh: wisp-js only in net-wisp.mjs; none in the ${reach.size} files index.mjs reaches`);
JS

VERSION=$(cd "$WEB" && node -p "require('./package.json').version ?? '0.0.0'" 2>/dev/null || echo 0.0.0)
NAME=@gexemu/runtime DESC="Linux programs in the browser: a kernel, the programs it runs, and their files"
[ $MODE = mp ] || NAME=@gexemu/runtime-single DESC="Linux programs in the browser, on a page without SharedArrayBuffer: a kernel, the programs it runs, and their files"
cat > "$OUT/package.json" <<PKG
{
  "name": "$NAME",
  "version": "$VERSION",
  "description": "$DESC",
  "license": "AGPL-3.0-only",
  "repository": { "type": "git", "url": "git+https://github.com/HeyPuter/gex.git", "directory": "page/runtime" },
  "homepage": "https://github.com/HeyPuter/gex/tree/master/page/runtime#readme",
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
# (ports/mesa/build.sh, ports/xwayland/build.sh); Emscripten's from the
# emsdk that builds this, Rust's from the tree's toolchain, the crates' from
# cargo's sources.
TP=$OUT/THIRD-PARTY
in_pkg() { (cd "$OUT" && for g; do for f in $g; do if [ -f "$f" ]; then printf '%s ' "$f"; fi; done; done); }
lic() { [ -f "$1" ] || { echo "page/runtime/build.sh: no $1 ($2's licence)" >&2; exit 1; }; }
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
    L=$MESA/licenses
    section "Mesa" "$REPLAY" "Mesa's docs/license.rst and licenses/" "$L/mesa/license.rst" "$L/mesa/MIT" "$L/mesa/Apache-2.0" "$L/mesa/BSL-1.0" "$L/mesa/SGI-B-2.0"
    section "Dawn and Tint" "$REPLAY" "Dawn's LICENSE" "$L/dawn/LICENSE"
    section "emdawnwebgpu" "$REPLAY" "emdawnwebgpu's webgpu_cpp/LICENSE" "$L/emdawnwebgpu/LICENSE"
fi
if [ -n "$XWL" ]; then
    L=$PUB/$XW/licenses
    section "Xwayland (the X.Org X server)" "$XWL" "xserver's COPYING" "$L/xserver"
    section "libwayland" "$XWL" "wayland's COPYING" "$L/wayland"
    for n in pixman libxcvt libxtrans libXau libfontenc libXfont2 libxkbfile zlib; do
        section "$n" "$XWL" "$n's licence file" "$L/$n"
    done
fi
[ -z "$XKM" ] || section "xkeyboard-config" "$XKM" "the build host's xkeyboard-config copyright" "$PUB/$XW/licenses/xkeyboard-config"
EMLIB=
if [ -n "$REPLAY$XWL$ZSTDW$PROV" ]; then
    command -v emcc > /dev/null || { echo "page/runtime/build.sh: no emcc: Emscripten's licences" >&2; exit 1; }
    EM=$(dirname "$(readlink -f "$(command -v emcc)")")
    EMLIB=$EM/system/lib
    section "Emscripten's runtime and system libraries (libc++, libc++abi, libunwind, compiler-rt)" "$REPLAY$XWL$ZSTDW" "emscripten's LICENSE and system/lib's" \
        "$EM/LICENSE" "$EMLIB/libcxx/LICENSE.TXT" "$EMLIB/libcxxabi/LICENSE.TXT" "$EMLIB/libunwind/LICENSE.TXT" "$EMLIB/compiler-rt/LICENSE.TXT"
    section "musl libc (Emscripten's libc$( [ -z "$PROV" ] || echo "; the printf providers' formatting core"))" "$REPLAY$XWL$ZSTDW$PROV" "emscripten's system/lib/libc/musl/COPYRIGHT" "$EMLIB/libc/musl/COPYRIGHT"
    lic "$EMLIB/dlmalloc.c" dlmalloc
    DL=$STAGE/dlmalloc-notice
    grep -m1 -B1 -A3 'released to the public domain' "$EMLIB/dlmalloc.c" > "$DL" || { echo "page/runtime/build.sh: no public-domain notice in $EMLIB/dlmalloc.c" >&2; exit 1; }
    section "dlmalloc (Emscripten's malloc$( [ -z "$PROV" ] || echo "; the heap providers'"))" "$REPLAY$XWL$ZSTDW$PROV" "the notice in emscripten's system/lib/dlmalloc.c" "$DL"
fi
[ -z "$ZSTD" ] || section "zstddec" "$ZSTD" "zstddec's LICENSE" "$WEB/vendor/zstddec/zstddec-LICENSE"
WISP=$(in_pkg 'net-wisp.mjs')
[ -z "$WISP" ] || section "wisp-js 0.4.1 (MercuryWorkshop)" "$WISP" "the package's LICENSE (src/net/vendor/wisp-js/SOURCE)" "$RT/src/net/vendor/wisp-js/LICENSE"
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
    if (!/Apache-2\.0 WITH LLVM-exception/.test(p.license ?? '')) throw new Error(`page/runtime/build.sh: ${p.name} ${p.version}: no licence file`);
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

# No file of the package names the machine it was built on: the builder's
# home, this tree's absolute path (its target directories with it), a
# CARGO_TARGET_DIR or TMPDIR of its own, or a /tmp/USER build directory,
# in any file, wasm included, fails the build, naming the file and the
# string. Only these: a scan of every absolute-looking string needs a list
# of the guest's own paths, and a wasm's strings sit one after another with
# no terminator, so what follows a guest path changes with every link.
node - "$OUT" "$(dirname "$WEB")" <<'JS'
const fs = require('fs'), os = require('os'), path = require('path');
const [out, tree] = process.argv.slice(2);
const real = (p) => { try { return fs.realpathSync(p); } catch { return p; } };
const user = os.userInfo().username;
const dirs = [os.homedir(), process.env.HOME, tree, real(tree), process.env.CARGO_TARGET_DIR,
  ...[process.env.TMPDIR].filter((d) => d && !/^\/tmp\/?$/.test(d))]
  .filter(Boolean).map((d) => d.replace(/\/+$/, '')).filter((d) => d.split('/').length > 2);
// A directory as itself or with a path below it; /tmp/USER as the start of one (/tmp/USER-1004/...).
const marks = [...new Set(dirs)].map((d) => new RegExp(`${d.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}(?![^/\0-\x20"'\`])`));
marks.push(new RegExp(`/tmp/${user.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`));
let bad = 0;
for (const f of fs.readdirSync(out).sort()) {
  const s = fs.readFileSync(path.join(out, f)).toString('latin1');
  const seen = new Set();
  for (const m of marks) {
    const at = s.search(m);
    if (at >= 0) seen.add(s.slice(at, at + 160).match(/^[^\0-\x1f"'`]*/)[0]);
  }
  for (const p of seen) {
    if ([...seen].some((q) => q !== p && q.endsWith(p))) continue; // one path, found by two of its directories
    console.error(`page/runtime/build.sh: ${f} names a build machine's path: ${p}`);
    bad++;
  }
}
if (bad) { console.error(`page/runtime/build.sh: ${bad} host paths in the package`); process.exit(1); }
JS
echo "page/runtime/build.sh: $OUT"
