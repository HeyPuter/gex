#!/bin/sh
# An ejected program's web build (crates/gex-eject/README.md): gex's app
# library (page/src/app.js) built with vite into DIR as app.mjs, its chunks
# under assets/, and beside it under gex/ the x86 shared-memory shim, kernel
# and seed (with the aarch64 set, the export providers), stripped of their
# names and DWARF unless GEX_KEEP_NAMES=1, Mesa (the wgpu
# replay, ports/mesa/build.sh) for programs with OpenGL, the X server
# (ports/xwayland/build.sh) for X11 programs, and under vendor/ the zstd
# decoder. The page is the user's
# (user/index.html imports app.mjs); there is no nt.html, and the build
# fails if anything in DIR names it.
#
#   site/build-eject-page.sh DIR
set -e
OUT=$(realpath -m "${1:?usage: site/build-eject-page.sh DIR}")
WEB=$(cd "$(dirname "$0")/../page" && pwd)
mkdir -p "$OUT"
# GEX_PAGE_ONE_FILE=1: app.mjs one file, its lazy chunks inlined (as each
# Worker is): a page that fetches everything its session loads up front
# (the KDE site) then has app.mjs and one file per Worker, none importing
# another, and no chunk imports app.mjs back (a second copy of it).
ONE=false; [ "${GEX_PAGE_ONE_FILE:-}" = 1 ] && ONE=true
# GEX_PAGE_MODE=mp|sp (vite.config.js): one build's code only; under mp the
# single-thread X server (xwayland-st/) is not copied either. Unset: both.
MODE=${GEX_PAGE_MODE:-}
CONFIG=$(mktemp --suffix=.mjs -p "$WEB")
trap 'rm -f "$CONFIG"' EXIT
cat > "$CONFIG" <<CFG
import base from '$WEB/vite.config.js';
export default {
  ...base,
  root: '$WEB',
  // URLs relative to app.mjs, so the site works under any path (a host's
  // /labs/NAME/ as at its root): Workers and chunks are found beside it.
  base: './',
  publicDir: false,
  logLevel: 'warn',
  build: { ...base.build, outDir: '$OUT', emptyOutDir: true,
           rollupOptions: {
             input: { app: '$WEB/src/app.js' },
             // A library: app.mjs keeps its exports (start, fromLaunch).
             preserveEntrySignatures: 'strict',
             output: { entryFileNames: (c) => (c.name === 'app' ? 'app.mjs' : 'assets/[name]-[hash].js'),
                       inlineDynamicImports: $ONE },
           } },
};
CFG
(cd "$WEB" && node_modules/.bin/vite build --config "$CONFIG")
mkdir -p "$OUT/gex" "$OUT/vendor"
cp "$WEB/vendor/zstddec/zstddec-stream.mjs" "$WEB/vendor/zstddec/zstddec.wasm" "$WEB/vendor/zstddec/zstddec-LICENSE" "$OUT/vendor/"
for f in gex_browser.x86.shared.wasm gex_kernel.x86.wasm seed.x86.shared.wasm; do
    cp "$WEB/public/gex/$f" "$OUT/gex/$f"
done
# The memory64 shim and seed, for a program that runs in a 64-bit memory
# (runtime.js fetches them then), when this tree has them (GEX_MEM64=1
# hosts/browser/build.sh)
for f in gex_browser.x86.m64.shared.wasm seed.x86.m64.shared.wasm; do
    rm -f "$OUT/gex/$f"
    [ ! -f "$WEB/public/gex/$f" ] || cp "$WEB/public/gex/$f" "$OUT/gex/$f"
done
# The aarch64 set, for a build whose guest is arm64 (start's build.arch),
# when this tree has it (GEX_ARCH=aarch64 hosts/browser/build.sh)
for f in gex_browser.shared.wasm gex_kernel.wasm seed.shared.wasm; do
    rm -f "$OUT/gex/$f"
    [ ! -f "$WEB/public/gex/$f" ] || cp "$WEB/public/gex/$f" "$OUT/gex/$f"
done
# The export providers (glibc's mem*/str*, libm: page/build-assets.sh), which
# the page binds to a program's glibc (arm64 or x86-64) when it has no unit
# cache of its own (runtime.js `cgen`). Without them the program runs
# glibc's own code.
rm -rf "$OUT/gex/cgen"
if ls "$WEB/public/gex/cgen/providers/"*.wasm > /dev/null 2>&1; then
    mkdir -p "$OUT/gex/cgen/providers"
    cp "$WEB/public/gex/cgen/providers/"*.wasm "$OUT/gex/cgen/providers/"
else echo "build-eject-page: no page/public/gex/cgen/providers (page/build-assets.sh): programs run without their providers" >&2; fi
for f in gex-wgpu-replay.js gex-wgpu-replay.wasm; do
    if [ -f "$WEB/public/gex/$f" ]; then cp "$WEB/public/gex/$f" "$OUT/gex/$f"
    else echo "build-eject-page: no page/public/gex/$f (ports/mesa/build.sh): the build has no OpenGL" >&2; fi
done
# The X server for X11 programs (runtime.js: gex/xwayland/, and
# gex/xwayland-st/ in the single-thread build; ports/xwayland/build.sh).
# Without it an X11 program stops at "x11 not available".
for d in xwayland xwayland-st; do
    rm -rf "$OUT/gex/$d"
    [ "$MODE" = mp ] && [ $d = xwayland-st ] && continue
    if [ -f "$WEB/public/gex/$d/Xwayland.mjs" ]; then cp -R "$WEB/public/gex/$d" "$OUT/gex/$d"
    elif [ "$d" = xwayland ]; then echo "build-eject-page: no page/public/gex/$d (ports/xwayland/build.sh): the build runs no X11 programs" >&2; fi
done
# A release page's wasm without its names and DWARF (nine tenths of the
# shim and kernel; user 10-30: every release build), as page/runtime/build.sh
# and page/build.sh strip theirs: the copies here, page/public keeps the
# originals to symbolize with. Only those sections go, so the providers'
# gex.* ones stay; Mesa's split-off half is left whole, as
# page/build.sh leaves it. GEX_KEEP_NAMES=1 keeps them, for a debug page.
if [ -z "${GEX_KEEP_NAMES:-}" ]; then
    command -v wasm-tools > /dev/null || { echo "build-eject-page: no wasm-tools to strip with (or GEX_KEEP_NAMES=1)" >&2; exit 1; }
    for w in "$OUT"/gex/gex_browser*.wasm "$OUT"/gex/gex_kernel*.wasm "$OUT"/gex/cgen/providers/*.wasm \
             "$OUT"/gex/gex-wgpu-replay.wasm "$OUT"/gex/xwayland*/*.wasm; do
        [ -f "$w" ] || continue
        wasm-tools strip -d '^name$' -d '^\.debug_.*' -d '^external_debug_info$' -d '^sourceMappingURL$' "$w" -o "$w.strip"
        mv -f "$w.strip" "$w"
    done
fi
# The compositor bridge also zstd'd: the X server's Worker asks for
# gexhostwl.wasm.zst first (hostx.js moduleBytes), 15 MB down to under 3.
for w in "$OUT"/gex/xwayland*/gexhostwl.wasm; do
    if [ -f "$w" ]; then zstd -q -f -19 "$w" -o "$w.zst"; fi
done
# Mesa's cold part, when the build split one off (ports/mesa/split.py).
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
