#!/bin/sh
# An ejected program's web build (crates/gex-eject/README.md): gex's app
# library (web/src/app.js) built with vite into DIR as app.mjs, its chunks
# under assets/, and beside it under gex/ the x86 shared-memory shim, kernel
# and seed (with the aarch64 set, the export providers), Mesa (the wgpu
# replay, web/build-mesa-wasm.sh) for programs with OpenGL, the X server
# (web/build-xwayland-wasm.sh) for X11 programs, and under vendor/ the zstd
# decoder. The page is the user's
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
             // A library: app.mjs keeps its exports (start, fromLaunch).
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
# The memory64 shim and seed, for a program that runs in a 64-bit memory
# (runtime.js fetches them then), when this tree has them (GEX_MEM64=1
# shims/browser/build.sh)
for f in gex_browser.x86.m64.shared.wasm seed.x86.m64.shared.wasm; do
    rm -f "$OUT/gex/$f"
    [ ! -f "$WEB/public/gex/$f" ] || cp "$WEB/public/gex/$f" "$OUT/gex/$f"
done
# The aarch64 set, for a build whose guest is arm64 (start's build.arch),
# when this tree has it (GEX_ARCH=aarch64 shims/browser/build.sh)
for f in gex_browser.shared.wasm gex_kernel.wasm seed.shared.wasm; do
    rm -f "$OUT/gex/$f"
    [ ! -f "$WEB/public/gex/$f" ] || cp "$WEB/public/gex/$f" "$OUT/gex/$f"
done
# The export providers (glibc's mem*/str*, libm: web/build-assets.sh), which
# the page binds to an arm64 program's glibc when it has no unit cache of its
# own (runtime.js `cgen`). Without them the program runs glibc's own code.
rm -rf "$OUT/gex/cgen"
if [ -f "$WEB/public/gex/gex_browser.shared.wasm" ]; then
    if ls "$WEB/public/gex/cgen/providers/"*.wasm > /dev/null 2>&1; then
        mkdir -p "$OUT/gex/cgen/providers"
        cp "$WEB/public/gex/cgen/providers/"*.wasm "$OUT/gex/cgen/providers/"
    else echo "build-eject-page: no web/public/gex/cgen/providers (web/build-assets.sh): arm64 programs run without their providers" >&2; fi
fi
for f in gex-wgpu-replay.js gex-wgpu-replay.wasm; do
    if [ -f "$WEB/public/gex/$f" ]; then cp "$WEB/public/gex/$f" "$OUT/gex/$f"
    else echo "build-eject-page: no web/public/gex/$f (web/build-mesa-wasm.sh): the build has no OpenGL" >&2; fi
done
# The X server for X11 programs (runtime.js: gex/xwayland/, and
# gex/xwayland-st/ in the single-thread build; web/build-xwayland-wasm.sh).
# Without it an X11 program stops at "x11 not available".
for d in xwayland xwayland-st; do
    rm -rf "$OUT/gex/$d"
    if [ -f "$WEB/public/gex/$d/Xwayland.mjs" ]; then cp -R "$WEB/public/gex/$d" "$OUT/gex/$d"
    elif [ "$d" = xwayland ]; then echo "build-eject-page: no web/public/gex/$d (web/build-xwayland-wasm.sh): the build runs no X11 programs" >&2; fi
done
# The compositor bridge also zstd'd: the X server's Worker asks for
# gexhostwl.wasm.zst first (hostx.js moduleBytes), 15 MB down to under 3.
for w in "$OUT"/gex/xwayland*/gexhostwl.wasm; do
    if [ -f "$w" ]; then zstd -q -f -19 "$w" -o "$w.zst"; fi
done
# Mesa's cold part, when the build split one off (web/mesa-split.py).
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
