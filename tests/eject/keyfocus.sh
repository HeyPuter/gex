#!/bin/bash
# The keyboard in an ejected W3 page when no window was activated: a Win32
# window that its client places and never activates got no keys at all
# (Momodora stayed at "Press Any Button"), because the page's compositor gives
# a placed window the keyboard only when its client says it is active. Now a
# key with no window focused goes to the topmost toplevel that is mapped and
# on the output, and never takes the keyboard from a window that has it.
# tests/eject/keyfocus.c is the program, tests/eject/keyfocus.mjs the checks.
#
#   tests/eject/keyfocus.sh --build W3_DIST W3_PREFIX   on the build host
#       (needs i686-w64-mingw32-gcc and web/public/gex with the x86 shim):
#       the program, an eject page (web/build-eject-page.sh) and a W3 site
#       around it (tests/eject/wine-site.py), in target/keyfocus
#   tests/eject/keyfocus.sh   on a bench host with a GPU (through
#       harness/bench/bench.sh): runs tests/eject/keyfocus.mjs on it
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
T=$ROOT/target/keyfocus
if [ "${1:-}" = --build ]; then
    dist=$(realpath "${2:?W3_DIST}"); prefix=$(realpath "${3:?W3_PREFIX}")
    rm -rf "$T"; mkdir -p "$T" || exit 1
    i686-w64-mingw32-gcc -O1 -Wall -o "$T/keyfocus.exe" "$ROOT/tests/eject/keyfocus.c" -lgdi32 || exit 1
    sh "$ROOT/web/build-eject-page.sh" "$T/site" > "$T/build.log" 2>&1 || { tail -5 "$T/build.log"; exit 1; }
    python3 "$ROOT/tests/eject/wine-site.py" "$T/site" "$dist" "$prefix" "$T/keyfocus.exe" 400 || exit 1
    cp "$ROOT/tests/eject/wine-site.html" "$T/site/index.html"
    echo "built $T"
    exit 0
fi
[ -f "$T/site/bundle.tar" ] || { echo "FAIL no page: run $0 --build W3_DIST W3_PREFIX on the build host first"; exit 1; }
cd "$ROOT"
timeout 600 node tests/eject/keyfocus.mjs "$T/site" "$@"
