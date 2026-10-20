#!/bin/sh
# glMapBufferRange's contents rules, natively (x86-64 guest, host GL): the
# probe checks itself on native Mesa first, then runs under gexc against
# guest/gl built for x86-64.
#
#   tests/gl-map-buffer.sh [gexc]      (default target/x86/release/gexc)
#       builds and runs in one go, on a host with a GPU
#   tests/gl-map-buffer.sh --build [gexc]   on the build host: the probe,
#       guest/gl and a copy of gexc in target/gl-map-buffer
#   tests/gl-map-buffer.sh --run   on a bench host with a GPU (through
#       harness/bench/bench.sh, -s target/gl-map-buffer): runs what --build left
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
SOURCE="$ROOT/tests/integration/programs/gl-map-buffer-probe.c"
T=$ROOT/target/gl-map-buffer
build() { # DIR
    cc -O2 -Wall -Wextra -o "$1/probe" "$SOURCE" -lEGL -lGL
    GEX_CC=${GEX_CC:-cc} "$ROOT/guest/gl/build.sh" "$1/guest-gl" >/dev/null
}
run() { # DIR GEXC
    EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 "$1/probe" | tee "$1/native.log"
    grep -q 'SUMMARY cases=15 passed=15 failed=0' "$1/native.log"
    "$2" run --gl --env LD_LIBRARY_PATH="$1/guest-gl" --env GEXGL_PROBE_VMDATA=1 "$1/probe" 2>"$1/gex.err" | tee "$1/gex.log"
    grep -q 'SUMMARY cases=15 passed=15 failed=0' "$1/gex.log"
    # The 64 MiB ASTC upload, tried twice, is reported once.
    [ "$(grep -c 'gexgl: dropped opcode' "$1/gex.err")" = 1 ] || { echo "FAIL gl-map-buffer: want one 'dropped opcode' line"; grep 'gexgl:' "$1/gex.err"; exit 1; }
    echo "ok   gl-map-buffer"
}
case "${1:-}" in
--build)
    rm -rf "$T"; mkdir -p "$T"
    build "$T"
    cp "${2:-$ROOT/target/x86/release/gexc}" "$T/gexc"
    echo "built $T" ;;
--run)
    [ -x "$T/gexc" ] || { echo "FAIL gl-map-buffer: run $0 --build on the build host first"; exit 1; }
    # A bench host has no wasm-ld to link providers; the cgen cache goes with the build.
    export GEX_PROVIDERS=0 GEX_CGEN_CACHE="$T/cgen-cache"
    run "$T" "$T/gexc" ;;
*)
    WORK=${WORK:-$(mktemp -d)}
    mkdir -p "$WORK"
    build "$WORK"
    run "$WORK" "${1:-$ROOT/target/x86/release/gexc}" ;;
esac
