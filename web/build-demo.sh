#!/bin/bash
# The demo build: the desktop page into web/dist-demo,
# always stripped, runtime modules shipped as .wasm.zst. No test runs on this
# build.
#
#   web/build-demo.sh [--no-runtime] [--no-x]
#
# 1. The runtime the page loads (unless --no-runtime): the shared shim, the
#    kernel and the seed (shims/browser/build.sh), and the export providers
#    (gexc cgen-image --providers), into web/public/gex as build-assets.sh does.
# 2. Xwayland, threaded (web/build-all.sh --x-only; skipped while its stamp
#    matches), into web/public/gex/xwayland.
# 3. The guest side is the debdemo repository's (`make desktop`): the base
#    rootfs, web/public/rootfs/base.tar.zst, and the launcher's packages,
#    web/public/pkgs (and web/public/gex-apt). They must be there already.
# 4. A staged public tree with only what the desktop loads; every .wasm
#    stripped (`wasm-tools strip`: DWARF and other custom sections out, the
#    name section kept; the seed and providers keep their gex.* sections) and
#    compressed (`zstd --ultra -22`) to NAME.wasm.zst, the plain file
#    removed. Providers stay plain: a Worker reads them synchronously.
# 5. vite with GEX_DEMO=1 (desktop.html is the page) and VITE_GEX_ZWASM=1
#    (zfetch.js decodes the .wasm.zst files).
#
# Run inside a resource scope:
#   systemd-run --user --scope -p TasksMax=4096 -p MemoryMax=32G nice -n 19 web/build-demo.sh
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CACHE="${GEX_WEB_BUILD:-$ROOT/target/web-build}"
OUT=$HERE/dist-demo
PUB=$HERE/public
RUNTIME=1 X=1
for a in "$@"; do
    case $a in
    --no-runtime) RUNTIME=0 ;;
    --no-x) X=0 ;;
    *) echo "usage: web/build-demo.sh [--no-runtime] [--no-x]" >&2; exit 2 ;;
    esac
done
say() { printf '\n== %s\n' "$*"; }
size() { stat -c %s "$1"; }
mib() { awk -v n="$1" 'BEGIN { printf "%.1f MiB", n / 1048576 }'; }
command -v wasm-tools >/dev/null || { echo "need wasm-tools (cargo install wasm-tools)" >&2; exit 1; }
command -v zstd >/dev/null || { echo "need zstd" >&2; exit 1; }
mkdir -p "$CACHE" "$PUB/gex"
[ -f "$PUB/rootfs/base.tar.zst" ] || { echo "no web/public/rootfs/base.tar.zst: build it with debdemo (make desktop GEX_DIR=$ROOT)" >&2; exit 1; }

# -- 1. runtime ----------------------------------------------------------------
if [ "$RUNTIME" = 1 ]; then
    say "runtime: shim, kernel, seed"
    (cd "$ROOT" && shims/browser/build.sh >"$CACHE/demo-shim.log" 2>&1) ||
        { tail -20 "$CACHE/demo-shim.log"; exit 1; }
    say "export providers"
    cargo build --release --manifest-path "$ROOT/Cargo.toml" -p gexc >/dev/null 2>&1
    rm -rf "$CACHE/demo-provcache"
    GEX_CGEN_CACHE="$CACHE/demo-provcache" "$ROOT/target/release/gexc" cgen-image --providers >/dev/null 2>&1
    mkdir -p "$PUB/gex/cgen/providers"
    rm -f "$PUB/gex/cgen/providers/"*.wasm
    cp "$CACHE/demo-provcache/providers/"*.wasm "$PUB/gex/cgen/providers/"
fi

# -- 2. Xwayland -----------------------------------------------------------------
if [ "$X" = 1 ]; then
    "$HERE/build-all.sh" --mp --x-only
fi

# -- 2b. the guest's GL marshallers (glibc, arm64) ------------------------------
# For the gears item: libGL/libEGL/libGLESv2 that marshal to the page's Mesa
# (guest/gl). A cross gcc when there is one, else zig cc for aarch64 glibc.
# web/build-assets.sh makes them; they are built here only when missing.
if ! [ -f "$PUB/fs/glibc/libGL.so.1" -a -f "$PUB/fs/glibc/libEGL.so.1" -a -f "$PUB/fs/glibc/libGLESv2.so.2" ]; then
    GL_CC=${GEX_GL_CC:-$(command -v aarch64-linux-gnu-gcc || true)}
    if [ -z "$GL_CC" ] && [ -x "$HOME/toolchains/zig/zig" ]; then
        GL_CC=$CACHE/aarch64-gnu-cc
        printf '#!/bin/sh\nexec %s cc -target aarch64-linux-gnu.2.36 "$@"\n' "$HOME/toolchains/zig/zig" > "$GL_CC"
        chmod +x "$GL_CC"
    fi
    if [ -n "$GL_CC" ]; then
        say "GL marshallers (glibc)"
        GEX_CC=$GL_CC "$ROOT/guest/gl/build.sh" "$CACHE/gldriver-glibc" >"$CACHE/demo-gl.log" 2>&1 ||
            { tail -20 "$CACHE/demo-gl.log"; exit 1; }
        mkdir -p "$PUB/fs/glibc"
        cp "$CACHE"/gldriver-glibc/{libGLESv2.so.2,libEGL.so.1,libGL.so.1} "$PUB/fs/glibc/"
    else
        echo "no aarch64 glibc compiler (aarch64-linux-gnu-gcc or ~/toolchains/zig): no GL marshallers" >&2
    fi
fi

# -- 4. stage --------------------------------------------------------------------------------
say "staging"
STAGE=$CACHE/public-demo
rm -rf "$STAGE"
mkdir -p "$STAGE/gex/cgen" "$STAGE/rootfs"
for f in gex_browser.shared.wasm gex_kernel.wasm seed.shared.wasm; do
    [ -f "$PUB/gex/$f" ] || { echo "no web/public/gex/$f: run without --no-runtime" >&2; exit 1; }
    cp "$PUB/gex/$f" "$STAGE/gex/"
done
cp -r "$PUB/gex/cgen/providers" "$STAGE/gex/cgen/"
[ -f "$PUB/gex/xwayland/Xwayland.wasm" ] || { echo "no web/public/gex/xwayland: run without --no-x" >&2; exit 1; }
cp -r "$PUB/gex/xwayland" "$STAGE/gex/"
cp "$PUB/rootfs/base.tar.zst" "$STAGE/rootfs/"
# Mesa (the host's GL, gex_wgpu), fetched only when a GL program starts.
if [ -f "$PUB/gex/gex-wgpu-replay.wasm" ]; then
    cp "$PUB/gex/gex-wgpu-replay.js" "$PUB/gex/gex-wgpu-replay.wasm" "$STAGE/gex/"
    # And its cold part, when the build split it (web/mesa-split.py). Plain,
    # not .zst: a miss before the prefetch has landed loads it with a
    # synchronous XHR (web/gex-wgpu-split.js), which zfetch.js cannot decode,
    # and without the file the static server answers index.html -- every GL
    # client then failed its first cold call and never drew a frame.
    if [ -f "$PUB/gex/gex-wgpu-replay.deferred.wasm" ]; then
        wasm-tools strip "$PUB/gex/gex-wgpu-replay.deferred.wasm" -o "$STAGE/gex/gex-wgpu-replay.deferred.wasm"
    fi
else
    echo "  no web/public/gex/gex-wgpu-replay.wasm (web/build-all.sh --mp --gl-only): GL items will not draw" >&2
fi
[ -d "$PUB/pkgs" ] && cp -r "$PUB/pkgs" "$STAGE/"
[ -d "$PUB/gex-apt" ] && cp -r "$PUB/gex-apt" "$STAGE/"
before=0 stripped=0 packed=0
for w in $(find "$STAGE/gex" -name '*.wasm' -not -path '*/cgen/*' -not -name '*.deferred.wasm'); do
    b=$(size "$w"); before=$((before + b))
    case ${w##*/} in
    seed*.wasm) wasm-tools strip -d '^\.debug' "$w" -o "$w.s" ;;  # keeps gex.* and name
    *) wasm-tools strip "$w" -o "$w.s" ;;                          # keeps name
    esac
    mv -f "$w.s" "$w"
    s=$(size "$w"); stripped=$((stripped + s))
    zstd --ultra -22 -T0 -q -f --rm "$w" -o "$w.zst"
    z=$(size "$w.zst"); packed=$((packed + z))
    printf '  %-34s %10s -> %10s stripped -> %10s zstd\n' "${w#"$STAGE"/}" "$(mib $b)" "$(mib $s)" "$(mib $z)"
done
echo "  runtime modules: $(mib $before) -> $(mib $stripped) stripped -> $(mib $packed) zstd"

# -- 5. the page ---------------------------------------------------------------------------
say "web/dist-demo"
rm -rf "$OUT"
(cd "$HERE" && GEX_DEMO=1 VITE_GEX_ZWASM=1 GEX_PUBLIC_DIR="$STAGE" GEX_OUT_DIR="$OUT" \
    npm run --silent build >"$CACHE/vite-demo.log" 2>&1) || { tail -20 "$CACHE/vite-demo.log"; exit 1; }
mv "$OUT/desktop.html" "$OUT/index.html"
rm -rf "$STAGE"
echo "  web/dist-demo: $(du -sh --apparent-size "$OUT" | cut -f1)"
echo "  first load (runtime + page + base rootfs):" \
     "$(mib $(( $(find "$OUT" -type f -not -path '*/pkgs/*' -not -path '*/gex-apt/*' -not -name 'gex-wgpu-replay.*' -printf '%s\n' | paste -sd+ | bc) )))"
