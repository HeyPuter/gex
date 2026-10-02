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

# Emscripten, found once and exported, so the scripts below agree about which
# one they are using -- they have historically disagreed, one defaulting to a
# home directory and another to /usr/lib.
if [ -z "${EMSDK:-}" ] || [ ! -f "${EMSDK}/emsdk_env.sh" ]; then
    emcc_on_path=$(command -v emcc 2>/dev/null || true)
    case "$emcc_on_path" in
        */upstream/emscripten/emcc) EMSDK=${emcc_on_path%/upstream/emscripten/emcc} ;;
    esac
    for candidate in "${EMSDK:-}" /usr/lib/emsdk "$HOME/emsdk"; do
        [ -n "$candidate" ] || continue
        [ -f "$candidate/emsdk_env.sh" ] || continue
        EMSDK=$candidate
        break
    done
    [ -z "${EMSDK:-}" ] || export EMSDK
fi
[ -z "${EMSDK:-}" ] || echo "Emscripten: $EMSDK"

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

# 2. Mesa for wasm, the command decoder (gex-wgpu-replay), is built by
#    build-mesa-wasm.sh, which needs Tint and emdawnwebgpu.

# 3. The guest's own GL libraries. These are *committed*, so a checkout
#    already has ones that match the tree; they are rebuilt here only because
#    a protocol change touches both halves, and a tree with local changes to
#    the marshaller would otherwise ship the old ones. Failing to build them
#    is therefore a note, not an error -- unlike the host modules above,
#    which nothing else can provide.
say "the guest's GL marshaller (committed; rebuilt for local changes)"
guest_out="${TMPDIR:-/tmp}/gex-guest-gl"
publish_guest_gl() {
    cp "$guest_out/libGLESv2.so.2" "$guest_out/libEGL.so.1" \
       "$guest_out/libGL.so.1" "$HERE/public/fs/glibc/" &&
        echo "  web/public/fs/glibc: refreshed"
}
# Tried, not interrogated. A cross compiler that compiles a trivial file can
# still fail the real build -- a wrapper whose `cc1` is missing does exactly
# that, and only on a compile that reaches the backend -- so the test is the
# build itself, and the next candidate is tried when the artifacts are not
# there afterwards. On Bedrock Linux the one that works lives in another
# stratum, which is the last attempt below.
rm -rf "$guest_out"
guest_built=""
for attempt in path strat; do
    [ -z "$guest_built" ] || break
    case "$attempt" in
    path)
        cc=${GEX_CC:-aarch64-linux-gnu-gcc}
        command -v "$cc" >/dev/null 2>&1 || continue
        echo "  trying $cc"
        GEX_CC="$(command -v "$cc")" "$ROOT/guest/gl/build.sh" "$guest_out" \
            >/dev/null 2>&1 || true
        ;;
    strat)
        command -v strat >/dev/null 2>&1 || continue
        strat debian test -x /usr/bin/aarch64-linux-gnu-gcc 2>/dev/null || continue
        echo "  trying strat debian aarch64-linux-gnu-gcc"
        strat debian env GEX_CC=/usr/bin/aarch64-linux-gnu-gcc \
            "$ROOT/guest/gl/build.sh" "$guest_out" >/dev/null 2>&1 || true
        ;;
    esac
    # The artifacts decide, not the exit status: that build skips itself with
    # a zero exit when it finds no compiler, which is a success no caller
    # should believe.
    [ -f "$guest_out/libGL.so.1" ] && guest_built=$attempt
done
if [ -n "$guest_built" ]; then
    publish_guest_gl
else
    echo "  no cross compiler could build them; keeping the committed libraries"
fi

# The bundled site, so a reload does not serve the previous build.
say "the site"
"$HERE/refresh-dist.sh" || true

if [ -n "$skipped" ]; then
    printf '\nsomething was skipped:%b\n' "$skipped" >&2
    exit 1
fi
printf '\nthe page modules are current\n'
