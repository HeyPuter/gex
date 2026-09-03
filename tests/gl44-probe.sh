#!/bin/sh
# Generic gate for a self-checking GL probe.
#
#   tests/gl44-probe.sh <name>            # e.g. clear, state, msaa-blit
#   GEX_GL_QUERY=gl=mesa tests/gl44-probe.sh clear
#
# The probe is built twice from one source: once natively against Mesa, which
# establishes the expected SUMMARY line, and once for the guest, which is run
# through the browser and must produce the same line. That makes these the only
# results on the WebGPU path that are checked rather than looked at.
#
# Several probes in tests/integration/programs had no gate at all and so had
# never been run against anything. This exists so adding one costs nothing.
set -eu

[ $# -ge 1 ] || { echo "usage: $0 <probe-name>" >&2; exit 2; }
NAME=$1

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
SOURCE="$ROOT/tests/integration/programs/gl44-$NAME-probe.c"
[ -f "$SOURCE" ] || { echo "no such probe: $SOURCE" >&2; exit 2; }
WORK=${GEX_PROBE_WORK:-${TMPDIR:-/tmp}/gex-gl44-$NAME}
GUEST_CC=${GEX_CC:-/home/velzie/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc}
mkdir -p "$WORK" "$ROOT/web/public/fs" "$ROOT/web/dist/fs"

${CC:-cc} -O2 -Wall -Wextra -o "$WORK/mesa" "$SOURCE" -lEGL -lGL
# stderr as well as stdout: a probe that fails says so on stderr, and capturing
# only stdout left the log empty and the failure reported as "no output".
# GEX_NATIVE_REFERENCE tells a probe it is the reference run, so it can skip
# checks that are about gex's own behaviour rather than about GL. gl44-indexed
# asserts that object names come from one global space -- true in the guest,
# false on native Mesa, which has a namespace per object type -- and without
# this the reference run failed and the gate could never form an expectation.
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 GEX_NATIVE_REFERENCE=1 \
    "$WORK/mesa" 2>&1 | tee "$WORK/mesa.log"
# Whatever native Mesa reports is the expectation; the probe decides its own
# case count, so it is read back rather than hard-coded per probe.
# Two SUMMARY shapes are in use -- some probes print `passed=`, some do not --
# so the pattern accepts either and only insists on `failed=0`.
EXPECT=$(grep -oE 'SUMMARY cases=[0-9]+( passed=[0-9]+)? failed=0' "$WORK/mesa.log" | tail -1)
# Not every probe prints a SUMMARY -- gl44-state-probe ends with its own
# marker line instead. Falling back to the last line the native run produced
# keeps the gate generic rather than needing a pattern per probe.
if [ -z "$EXPECT" ]; then
    EXPECT=$(grep -v '^[[:space:]]*$' "$WORK/mesa.log" | tail -1)
    [ -n "$EXPECT" ] || { echo "native Mesa run produced no output" >&2; exit 1; }
    # Only the fallback needs this: a SUMMARY line matched above already says
    # failed=0, and "failed=0" contains "fail", so testing it here would
    # reject every passing probe -- which it did, until this moved.
    case $EXPECT in
        *fail*|*FAIL*|*error*|*Error*)
            echo "native Mesa run did not pass: $EXPECT" >&2; exit 1 ;;
    esac
fi
echo "expecting: $EXPECT"

GEX_CC="$GUEST_CC" "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
"$GUEST_CC" -O2 -Wall -Wextra -I"$ROOT/third_party/mobilegl-webgpu/include" \
    -o "$WORK/probe" "$SOURCE" "$WORK/guest-gl/libGLESv2.so.2"
# Refresh the host-side Mesa module too, not just the guest binary.
#
# `?gl=mesa` loads /gex/gex-wgpu-replay.*, and this gate used to copy only the
# guest probe -- so it tested whatever host driver another script had last
# copied in. A driver fix could be built, run, and appear to change nothing,
# which is exactly what happened: an integer-clear fix was called ineffective
# on the strength of a stale module.
MESA_BUILD=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}/build/src/gallium/targets/gex-wasm
if [ -f "$MESA_BUILD/gex-wgpu-replay.wasm" ]; then
    for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
        mkdir -p "$tree/gex"
        cp "$MESA_BUILD/gex-wgpu-replay.js" \
           "$MESA_BUILD/gex-wgpu-replay.wasm" "$tree/gex/"
    done
    # Newer than the driver source, or it is testing a driver that no longer
    # exists. This has now cost three separate investigations -- a stale vite
    # bundle had the browser reporting GL 4.0 against a native 4.4, a stale
    # generated extension table withheld ARB_viewport_array, and a stale host
    # module failed two compute cases that the current driver passes. The
    # module's own GL_VERSION carries the git sha, which is how the last one
    # was spotted; this says so before the run instead of after.
    DRIVER_SRC="$ROOT/third_party/mesa/src/gallium/drivers/gex_wgpu"
    for src in "$DRIVER_SRC"/gex_wgpu_screen.c "$DRIVER_SRC"/gex_wgpu_native.c; do
        [ -f "$src" ] || continue
        if [ "$src" -nt "$MESA_BUILD/gex-wgpu-replay.wasm" ]; then
            echo "warning: $(basename "$src") is newer than the staged host" \
                 "module; run web/build-mesa-wasm.sh or this tests an older" \
                 "driver than the native expectation" >&2
        fi
    done
else
    echo "warning: no gex-wgpu-replay in $MESA_BUILD; testing whatever host" \
         "module is already staged (run web/build-mesa-wasm.sh)" >&2
fi

for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    cp "$WORK/probe" "$tree/fs/gl44-$NAME-probe"
    cp "$WORK/guest-gl/libGLESv2.so.2" "$tree/fs/libGLESv2.so.2"
    cp "$WORK/guest-gl/libEGL.so.1" "$tree/fs/libEGL.so.1"
done

# GEX_GUEST_ENV=NAME=VALUE reaches the guest process as an environment
# variable -- GEXGL_EXTENSION_TRACE=1 is the one worth knowing about, which
# lists the host extensions the guest declines to repeat. Deliberately
# unquoted below so it expands to separate words; no value may contain a
# space.
#
# GEX_GUEST_NAMES=1 is always in the guest's environment: it tells a probe it
# is the guest, which is the only place object names come from one global pool.
# The distinction used to be carried by GEX_NATIVE_REFERENCE -- set on the
# reference run and not on the *native driver* run, both of which are native --
# so gl44-indexed-probe asserted a guest-only invariant against a native
# driver and stopped at its third case.
# `glenv` is repeatable and each value is one KEY=VALUE, so a second pair is a
# second --query, not a comma.
GUEST_ENV_ARGS="--query glenv=GEX_GUEST_NAMES=1"
[ -z "${GEX_GUEST_ENV:-}" ] ||
    GUEST_ENV_ARGS="$GUEST_ENV_ARGS --query glenv=$GEX_GUEST_ENV"

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
node "$ROOT/web/test/drive.mjs" gears --allow-errors \
    --query "${GEX_GL_QUERY:-gl=mesa}" \
    --query "glclient=/fs/gl44-$NAME-probe" \
    $GUEST_ENV_ARGS \
    --want "$EXPECT" --fail 'SUMMARY .*failed=[1-9]|FATAL' \
    --wall 120 --idle 60
