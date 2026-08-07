#!/bin/sh
# The guest's OpenGL, on a real browser's WebGL.
#
#   tests/browser-gl/run.sh [work-dir]
#
# Node cannot run this one. The other browser tests use node because what they
# check is the translated code, and V8 is V8 wherever it is; this checks that a
# guest's GL calls reach a canvas, and node has no canvas. So it drives
# headless Chromium and reads what the page printed.
#
# Two programs, both ordinary GLES2 built against the standard headers and
# linked against the usual sonames:
#
#   gles-triangle  a green triangle over a blue clear, checked pixel by pixel
#   gles-gears     the three gears, checked by how much of the frame they cover
#
# The whole point is that neither knows it is not talking to Mesa.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
WORK="${1:-${TMPDIR:-/tmp}/gex-browser-gl}"
GEXC="$ROOT/target/release/gexc"
SHIM="$ROOT/target/wasm32-unknown-unknown/release/gex_browser.wasm"
CC="${GEX_CC:-$HOME/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc}"
MUSL="$(dirname "$CC")/../aarch64-linux-musl/lib/libc.so"
CHROME="${GEX_CHROME:-$(command -v chromium || command -v chromium-browser || command -v google-chrome || true)}"

for need in "$GEXC" "$CC" "$CHROME"; do
    if [ -z "$need" ] || [ ! -x "$need" ]; then
        echo "skip: missing $need"
        exit 0
    fi
done
# Built through the shim's own script, not plain cargo: the memory has to be
# imported, the globals rebased above the guest, and `__stack_pointer`
# exported so the page can move the shim's stack out of the guest's image. A
# module built without those loads and then fails on the first call.
echo "== building the browser shim"
"$ROOT/shims/browser/build.sh" >/dev/null 2>&1 || true
[ -f "$SHIM" ] || { echo "skip: the browser shim did not build"; exit 0; }
[ -d /usr/include/GLES2 ] || { echo "skip: no GLES2 headers"; exit 0; }

mkdir -p "$WORK/include"
cp -r /usr/include/EGL /usr/include/GLES2 /usr/include/KHR "$WORK/include/" 2>/dev/null || true

echo "== building the guest's GL driver"
"$ROOT/guest/gl/build.sh" "$WORK/driver" >/dev/null

echo "== building the programs"
mkdir -p "$WORK/root/lib" "$WORK/root/usr/lib"
cp "$MUSL" "$WORK/root/lib/ld-musl-aarch64.so.1"
cp "$WORK/driver/libGLESv2.so.2" "$WORK/driver/libEGL.so.1" "$WORK/root/usr/lib/"
for prog in gles-triangle gles-gears; do
    "$CC" -O2 -o "$WORK/root/$prog" "$ROOT/tests/integration/programs/$prog.c" \
        -I "$WORK/include" -L "$WORK/driver" -lEGL -lGLESv2 -lm
done

# One module per program. `--no-shared-memory` because the page's runtime
# declares a plain memory, and `--exhaustive` is not needed: the dispatch miss
# pulls in what the guest actually reaches.
echo "== translating"
for prog in gles-triangle gles-gears; do
    "$GEXC" build --no-shared-memory "$WORK/root/$prog" -o "$WORK/$prog.wasm" >/dev/null
done

# Everything the page loads has to be same-origin, so it is served rather than
# opened from a file URL.
mkdir -p "$WORK/site/gex" "$WORK/site/src" "$WORK/site/fs"
cp "$SHIM" "$WORK/site/gex/gex_browser.wasm"
cp "$ROOT/shims/browser/js/gex.mjs" "$WORK/site/"
cp "$ROOT/web/src/gl.js" "$ROOT/web/src/gl-proto.js" "$WORK/site/src/"
for prog in gles-triangle gles-gears; do
    cp "$WORK/$prog.wasm" "$WORK/root/$prog" "$WORK/site/fs/"
done
cp "$WORK/root/lib/ld-musl-aarch64.so.1" "$WORK/site/fs/"
cp "$WORK/driver/libGLESv2.so.2" "$WORK/driver/libEGL.so.1" "$WORK/site/fs/"
cp "$HERE/page.html" "$WORK/site/index.html"

port=$((18000 + $$ % 2000))
pass=0
fail=0

for prog in gles-triangle gles-gears; do
    echo "== running $prog in Chromium"
    out="$WORK/$prog.out"
    rm -f "$out"
    python3 "$HERE/serve.py" "$WORK/site" "$port" "$out" >/dev/null 2>&1 &
    server=$!
    for _ in $(seq 50); do
        curl -fsS "http://127.0.0.1:$port/index.html" >/dev/null 2>&1 && break
        sleep 0.1
    done

    # SwiftShader because no GPU here is reachable from the sandbox; the path
    # through WebGL is the same either way. No virtual-time budget: the page
    # says when it is finished, and this waits for it to.
    timeout 600 "$CHROME" --headless --no-sandbox --disable-gpu \
        --enable-unsafe-swiftshader \
        "http://127.0.0.1:$port/index.html?prog=$prog" >/dev/null 2>&1 &
    browser=$!
    for _ in $(seq 600); do
        [ -s "$out" ] && break
        sleep 1
    done
    kill $browser 2>/dev/null || true
    kill $server 2>/dev/null || true
    wait $browser 2>/dev/null || true
    wait $server 2>/dev/null || true

    if [ ! -s "$out" ]; then
        echo "FAIL $prog: the page never reported"
        fail=$((fail + 1))
        continue
    fi
    sed 's/^/     /' "$out"
    if grep -q "^RESULT ok" "$out"; then
        pass=$((pass + 1))
    else
        echo "FAIL $prog"
        fail=$((fail + 1))
    fi
    port=$((port + 1))
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
