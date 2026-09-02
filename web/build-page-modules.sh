#!/bin/sh
# Rebuild the wasm the page loads, in the order their dependencies run.
#
#   web/build-page-modules.sh
#
# Everything under `web/public/gex/` is a build artifact and is *gitignored*,
# while the guest's GL libraries under `web/public/fs/glibc/` are committed.
# So a `git pull` moves the guest and leaves the host where it was, and the
# two then disagree about the command protocol -- which shows up as a black
# window and nothing else. (The guest does say so on its stderr: "the host's
# GL module speaks a different protocol version".) This script is the other
# half of that: one command that brings the host modules up to the tree.
#
# It builds what it can and says what it skipped, because the four have
# different prerequisites -- Emscripten for all of them, a Tint build and an
# emdawnwebgpu package for the WebGPU driver, an aarch64 cross compiler for
# the guest libraries. A partial refresh is still better than a stale one, and
# what was skipped is named rather than left to be discovered later.
set -eu

HERE=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
ROOT=$(CDPATH='' cd -- "$HERE/.." && pwd)
skipped=""

say() { printf '\n== %s\n' "$1"; }
skip() { skipped="$skipped\n  $1"; echo "  skipped: $1" >&2; }

# 1. The X server in the page, and the compositor shim beside it. The shim
#    carries the Wayland protocol machine and the X window manager, which is
#    what publishes a window to the compositor -- an old one here means an X
#    client that draws only through GL never appears at all.
say "the page's X server and host Wayland shim"
if command -v emcc >/dev/null 2>&1; then
    "$HERE/build-xwayland-wasm.sh" || skip "build-xwayland-wasm.sh failed"
else
    skip "no Emscripten for the X server"
fi

# 2. virglrenderer: the decoder for the virgl backend, and the frame store the
#    compositors draw from.
say "virglrenderer"
if command -v emcc >/dev/null 2>&1; then
    "$HERE/build-virgl-wasm.sh" || skip "build-virgl-wasm.sh failed"
else
    skip "no Emscripten for virglrenderer"
fi

# 3. Mesa for wasm: the command decoder itself. `gex-virgl-replay` always,
#    `gex-wgpu-replay` when that tree has been configured for WebGPU (see
#    build-mesa-wasm.sh, which needs Tint and emdawnwebgpu).
say "Mesa (the decoder), for both backends this tree has"
if command -v meson >/dev/null 2>&1 && command -v emcc >/dev/null 2>&1; then
    "$HERE/build-mesa-virgl-wasm.sh" || skip "build-mesa-virgl-wasm.sh failed"
else
    skip "no meson or Emscripten for Mesa"
fi

# 4. The guest's own GL libraries. Committed, so a checkout has them -- built
#    here too, because a protocol change touches both halves and building only
#    the host is the same mistake in the other direction.
say "the guest's GL marshaller"
if command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
    GEX_CC=$(command -v aarch64-linux-gnu-gcc) "$ROOT/guest/gl/build.sh" \
        "${TMPDIR:-/tmp}/gex-guest-gl" >/dev/null &&
        cp "${TMPDIR:-/tmp}/gex-guest-gl/libGLESv2.so.2" \
           "${TMPDIR:-/tmp}/gex-guest-gl/libEGL.so.1" \
           "${TMPDIR:-/tmp}/gex-guest-gl/libGL.so.1" \
           "$HERE/public/fs/glibc/" &&
        echo "  web/public/fs/glibc: refreshed" ||
        skip "the guest GL build failed"
elif command -v strat >/dev/null 2>&1 && strat debian test -x /usr/bin/aarch64-linux-gnu-gcc; then
    strat debian env GEX_CC=/usr/bin/aarch64-linux-gnu-gcc "$ROOT/guest/gl/build.sh" \
        "${TMPDIR:-/tmp}/gex-guest-gl" >/dev/null &&
        cp "${TMPDIR:-/tmp}/gex-guest-gl/libGLESv2.so.2" \
           "${TMPDIR:-/tmp}/gex-guest-gl/libEGL.so.1" \
           "${TMPDIR:-/tmp}/gex-guest-gl/libGL.so.1" \
           "$HERE/public/fs/glibc/" &&
        echo "  web/public/fs/glibc: refreshed" ||
        skip "the guest GL build failed"
else
    skip "no aarch64-linux-gnu-gcc for the guest GL libraries"
fi

# The bundled site, so a reload does not serve the previous build.
say "the site"
"$HERE/refresh-dist.sh" || true

if [ -n "$skipped" ]; then
    printf '\nsomething was skipped:%b\n' "$skipped" >&2
    exit 1
fi
printf '\nthe page modules are current\n'
