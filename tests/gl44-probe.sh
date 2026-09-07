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

# A probe that opens its own X connection needs -lX11 as well. Two do --
# glx-context and window-present -- and without it the *native reference*
# fails to link:
#
#   /usr/bin/ld: /usr/lib/libX11.so.6: error adding symbols: DSO missing
#   from command line
#
# which the gate reports as a probe FAIL indistinguishable from a driver one.
# Keyed off the source so no other probe takes the dependency.
PROBE_LIBS="-lEGL -lGL"
grep -q '<X11/' "$SOURCE" && PROBE_LIBS="$PROBE_LIBS -lX11"
# shellcheck disable=SC2086
${CC:-cc} -O2 -Wall -Wextra -o "$WORK/mesa" "$SOURCE" $PROBE_LIBS
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
# GEX_PROBE_EXPECT overrides it, for the probes where the *reference* is what
# fails. gl44-image-noformat-fs is one: llvmpipe returns uninitialised memory
# for a format-less image store to an r32f unit, radeonsi and this driver both
# return the value stored, and radeonsi is the tie-breaker. Without an
# override such a probe can never be browser-gated at all, because the gate
# insists its oracle passed first -- which is the right default and the wrong
# answer here.
#
#   GEX_PROBE_EXPECT='SUMMARY cases=2 passed=2 failed=0' #     tests/gl44-probe.sh image-noformat-fs
#
# A probe that needs the override **declares it in its own header**, in that
# exact shape, and this reads it back -- so the expectation lives in one place
# next to the reason for it, rather than in a table here that a new probe
# silently misses. Precedence: the environment, then the probe's own
# declaration, then whatever the native run reported.
#
# Without this the sweep reported FAIL for a probe that passed. The native
# reference for gl44-internalformat-negative is *stock* Mesa, which dies on
# the case being tested, so EXPECT fell back to the last line it managed to
# print -- a version string -- and the browser could never match it. A gate
# that says FAIL for a passing probe teaches you to skim the FAIL column.
#
# Declare one only where the correct answer is that the probe fully passes and
# the *reference* is what fails (image-noformat-fs, quad), or where a failing
# case is deliberate (two-contexts). Never to pin a device-dependent value:
# texture-sampler reports one sample count in the browser against llvmpipe's
# two, and freezing that would turn a real native-vs-browser signal into a
# permanent green.
DECLARED=$(sed -n "s/.*GEX_PROBE_EXPECT='\([^']*\)'.*/\1/p" "$SOURCE" | head -1)
if [ -n "$DECLARED" ]; then EXPECT=$DECLARED; fi
if [ -n "${GEX_PROBE_EXPECT:-}" ]; then EXPECT=$GEX_PROBE_EXPECT; fi
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
# Any failing case is a failure -- except where the expectation itself names
# the failures that are correct (gl44-two-contexts expects failed=1, because
# GL genuinely refuses part of what it asks). Contradicting the --want with
# the --fail would make such a probe unpassable.
case $EXPECT in
    *failed=0) FAIL_PATTERN='SUMMARY .*failed=[1-9]|FATAL' ;;
    *) FAIL_PATTERN='FATAL' ;;
esac

# A probe that needs GLX or Xlib cannot be built for the guest here, and that
# is about this machine rather than about the driver: the guest's libGL really
# does export 31 glX* entry points ([[glx-and-glmark2]]), but the aarch64-musl
# cross toolchain carries neither <GL/glx.h> nor the X11 headers it pulls in.
# Reported as an explicit skip, exit 3, so it cannot be read as a driver
# failure -- glx-context and window-present spent a whole browser sweep filed
# as FAIL next to real defects.
if grep -qE '<GL/glx\.h>|<X11/' "$SOURCE"; then
    if ! printf '#include <GL/glx.h>\nint main(void){return 0;}\n' |
         "$GUEST_CC" -x c -I"$ROOT/third_party/mobilegl-webgpu/include" \
             -fsyntax-only - 2>/dev/null; then
        echo "SKIP $NAME: needs GLX/X11 headers the guest toolchain has not got"
        exit 3
    fi
fi

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

# libGL.so.1 as well as the ES pair. guest/gl/build.sh produces all three as
# byte-identical copies of one object, but this staged only two -- so a probe
# run left libGL.so.1 behind at whatever the last full web/build-assets.sh had
# produced. A probe gate could then go green against a guest driver that no GLX
# application would ever load, and did: the negative-count fix passed here
# while the libGL.so.1 next to it was eighteen minutes stale.
for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
    cp "$WORK/probe" "$tree/fs/gl44-$NAME-probe"
    cp "$WORK/guest-gl/libGLESv2.so.2" "$tree/fs/libGLESv2.so.2"
    cp "$WORK/guest-gl/libEGL.so.1" "$tree/fs/libEGL.so.1"
    cp "$WORK/guest-gl/libGL.so.1" "$tree/fs/libGL.so.1"
done

# The glibc guest driver is a separate build -- the packaged desktop
# applications are Debian, while the probes and gears are musl -- and this gate
# only builds the musl one. So a probe can pass against a freshly built driver
# while Blender and FreeCAD next door load a stale one. Building both here
# would slow every probe run, so say it instead of hiding it.
GLIBC_GL="$ROOT/web/public/fs/glibc/libGL.so.1"
if [ -f "$GLIBC_GL" ] && [ "$ROOT/web/public/fs/libGL.so.1" -nt "$GLIBC_GL" ]; then
    echo "note: web/public/fs/glibc/ is older than the driver just staged." >&2
    echo "  The packaged desktop apps load that one -- run web/build-assets.sh" >&2
    echo "  before trusting an app gate." >&2
fi

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
# And the host's own environment, which is a different thing: `glenv` reaches
# the guest process, `mesaenv` reaches the driver inside the wasm module. Every
# GEX_WGPU_* diagnostic is getenv-gated, and getenv answered NULL in the
# browser until there was a way to set one -- so the instruments existed
# everywhere except the place their defects live.
#   GEX_MESA_ENV=GEX_WGPU_DRAW_DROP=1 tests/gl44-probe.sh image-targets
[ -z "${GEX_MESA_ENV:-}" ] ||
    GUEST_ENV_ARGS="$GUEST_ENV_ARGS --query mesaenv=$GEX_MESA_ENV"

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
node "$ROOT/web/test/drive.mjs" gears --allow-errors \
    --query "${GEX_GL_QUERY:-gl=mesa}" \
    --query "glclient=/fs/gl44-$NAME-probe" \
    $GUEST_ENV_ARGS \
    --want "$EXPECT" --fail "$FAIL_PATTERN" \
    --wall 120 --idle 60
