#!/bin/bash
# A press in the top band of an ejected page taller than its viewport: the
# page's bar (34 px) above a 1280x720 output in a 1280x720 viewport, so the
# canvas runs past the bottom and focusing it could scroll the page. The
# press must reach the guest where the pointer was, and the page must not
# scroll. (compositor.js onDown: focus() without preventScroll scrolled the
# page 34 px before the press was mapped, so the guest got it 34 px low.)
#
#   tests/eject/click-scroll.sh --build   on the build host (needs the X11
#       headers and web/public/gex with the x86 shim and Xwayland): the X11
#       client tests/x11/xinput-echo.c with its libraries in a bundle, and an
#       eject page (web/build-eject-page.sh) around it, in target/click-scroll
#   tests/eject/click-scroll.sh   on a bench host with a GPU (through
#       harness/bench/bench.sh): runs tests/eject/click-scroll.mjs on it
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
T=$ROOT/target/click-scroll
if [ "${1:-}" = --build ]; then
    rm -rf "$T"; mkdir -p "$T/stage/game" || exit 1
    cc -O2 -o "$T/stage/game/xinput-echo" "$ROOT/tests/x11/xinput-echo.c" -lX11 || exit 1
    # Its libraries at their host paths, the loader with them.
    for f in $(ldd "$T/stage/game/xinput-echo" | grep -o '/[^ ]*'); do
        mkdir -p "$T/stage$(dirname "$f")" && cp -L "$f" "$T/stage$f" || exit 1
    done
    mkdir -p "$T/stage/tmp" "$T/stage/home/g"
    (cd "$T/stage" && tar -cf "$T/bundle.tar" --format=gnu *) || exit 1
    rm -rf "$T/stage"
    sh "$ROOT/web/build-eject-page.sh" "$T/site" > "$T/build.log" 2>&1 || { tail -5 "$T/build.log"; exit 1; }
    [ -f "$T/site/gex/xwayland/Xwayland.mjs" ] || { echo "FAIL no Xwayland in the page (web/build-xwayland-wasm.sh)"; exit 1; }
    mv "$T/bundle.tar" "$T/site/bundle.tar"
    cp "$ROOT/tests/eject/click-scroll.html" "$T/site/index.html"
    echo "built $T"
    exit 0
fi
[ -f "$T/site/bundle.tar" ] || { echo "FAIL no page: run $0 --build on the build host first"; exit 1; }
cd "$ROOT"
timeout 300 node tests/eject/click-scroll.mjs "$T/site" "$@"
