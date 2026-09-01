#!/bin/sh
set -eu

ROOT="$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"
OUT="${TMPDIR:-/tmp}/gex-gl-sdl-interposer-test"

"${CC:-cc}" -std=c11 -O0 -g -Wall -Wextra -Werror \
    "$ROOT/tests/gl-sdl-interposer.c" "$ROOT/guest/gl/gexgl-sdl.c" \
    -ldl -o "$OUT"
"$OUT"
