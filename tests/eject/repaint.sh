#!/bin/bash
# Repaints reach the screen only when they change it, and carry only their
# damage: a client that commits the same pixels over and over draws no frame
# and uploads nothing (a canvas presented for each such commit costs a full
# frame a time), and a narrow damage rectangle is uploaded as its own pixels,
# not as the rows of the buffer it crosses (those of an 8-pixel column of a
# 1280-pixel buffer are 160 times its pixels). tests/eject/repaint.c is the client.
#
#   tests/eject/repaint.sh --build   on the build host (needs gcc and
#       web/public/gex with the x86 shim): the static client in a bundle, and
#       an eject page (web/build-eject-page.sh) around it, in target/repaint
#   tests/eject/repaint.sh [--swiftshader]   runs tests/eject/repaint.mjs on
#       it: on a bench host with a GPU (through harness/bench/bench.sh), or
#       anywhere headless with --swiftshader
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
T=$ROOT/target/repaint
if [ "${1:-}" = --build ]; then
    rm -rf "$T"; mkdir -p "$T/stage/game" "$T/stage/tmp" "$T/stage/home/g" || exit 1
    gcc -O1 -static -o "$T/stage/game/repaint" "$ROOT/tests/eject/repaint.c" || exit 1
    (cd "$T/stage" && tar -cf "$T/bundle.tar" --format=gnu *) || exit 1
    rm -rf "$T/stage"
    sh "$ROOT/web/build-eject-page.sh" "$T/site" > "$T/build.log" 2>&1 || { tail -5 "$T/build.log"; exit 1; }
    mv "$T/bundle.tar" "$T/site/bundle.tar"
    cp "$ROOT/tests/eject/repaint.html" "$T/site/index.html"
    echo "built $T"
    exit 0
fi
[ -f "$T/site/bundle.tar" ] || { echo "FAIL no page: run $0 --build on the build host first"; exit 1; }
cd "$ROOT"
timeout 300 node tests/eject/repaint.mjs "$T/site" "$@"
