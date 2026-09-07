#!/bin/sh
# Run one pinned Khronos OpenGL 4.4 CTS case through the persistent real-browser
# MobileGL/WebGPU path. Override GEX_CTS_CASE to grow the proven case set.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
BUILD=${GEX_CTS_BUILD:-$ROOT/web/.work/gl44-cts}
BIN=${GEX_CTS_BINARY:-}
CASE=${GEX_CTS_CASE:-KHR-GL44.info.vendor}
LOG_FILENAME=${GEX_CTS_LOG_FILENAME:-/tmp/glcts.qpa}
WRAPPER=${GEX_CTS_WRAPPER:-$BUILD/glcts-log-wrapper}

if [ -z "$BIN" ]; then
    BIN=$(find "$BUILD" -type f -name glcts -perm -0100 | head -n 1)
fi
[ -n "$BIN" ] && [ -x "$BIN" ] || {
    echo "no glcts binary; run tests/gl44-cts.sh first" >&2
    exit 1
}

# Which host GL backend executes the guest's stream. `mesa` -- Mesa's frontend
# on the gex_wgpu Gallium driver -- is the only accepted value. MobileGL used
# to be the default here; it is disabled everywhere now, so asking for it is an
# error rather than a silently different conformance answer.
case ${GEX_CTS_BACKEND:-mesa} in
    mesa)
        BACKEND_QUERY='gl=mesa'
        MESA_BUILD=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}/build/src/gallium/targets/gex-wasm
        [ -f "$MESA_BUILD/gex-wgpu-replay.wasm" ] || {
            echo "no gex-wgpu-replay; run web/build-mesa-wasm.sh" >&2
            exit 1
        }
        for tree in "$ROOT/web/public" "$ROOT/web/dist"; do
            mkdir -p "$tree/gex"
            cp "$MESA_BUILD/gex-wgpu-replay.js" \
               "$MESA_BUILD/gex-wgpu-replay.wasm" "$tree/gex/"
        done
        ;;
    *) echo "unknown GEX_CTS_BACKEND: ${GEX_CTS_BACKEND}" >&2; exit 2 ;;
esac

CTS_STAGE="$ROOT/web/public/fs/.glcts-stage-$$"
CTS_PUBLIC=
CTS_DIST=
WRAPPER_PUBLIC=
WRAPPER_DIST=
HARNESS_OUTPUT=
cleanup() {
    rm -f "$CTS_STAGE"
    [ -z "$CTS_PUBLIC" ] || rm -f "$CTS_PUBLIC"
    [ -z "$CTS_DIST" ] || rm -f "$CTS_DIST"
    [ -z "$WRAPPER_PUBLIC" ] || rm -f "$WRAPPER_PUBLIC"
    [ -z "$WRAPPER_DIST" ] || rm -f "$WRAPPER_DIST"
    [ -z "$HARNESS_OUTPUT" ] || rm -f "$HARNESS_OUTPUT"
}
trap cleanup EXIT INT TERM

cp "$BIN" "$CTS_STAGE"
if command -v patchelf >/dev/null 2>&1; then
    patchelf --remove-rpath "$CTS_STAGE"
fi
if [ -n "${GEX_CTS_TOOLCHAIN:-}" ]; then
    "$GEX_CTS_TOOLCHAIN/bin/aarch64-linux-musl-strip" "$CTS_STAGE"
fi
CTS_TAG=$(sha256sum "$CTS_STAGE" | cut -c1-16)
CTS_NAME="glcts-$CTS_TAG"
CTS_PUBLIC="$ROOT/web/public/fs/$CTS_NAME"
CTS_DIST="$ROOT/web/dist/fs/$CTS_NAME"
mv "$CTS_STAGE" "$CTS_PUBLIC"

# The log wrapper, from source, for the same reason as the marshaller below:
# $BUILD/glcts-log-wrapper is written only when the CTS is compiled, and the
# copy here was five days old while its source had changed. An instrument that
# does not track its source reports yesterday's answers.
if [ -n "${GEX_CTS_TOOLCHAIN:-}" ] && [ -x "$GEX_CTS_TOOLCHAIN/bin/aarch64-linux-musl-gcc" ]; then
    "$GEX_CTS_TOOLCHAIN/bin/aarch64-linux-musl-gcc" -O2 \
        "$ROOT/tests/cts/glcts-log-wrapper.c" -o "$BUILD/glcts-log-wrapper" || {
            echo "could not rebuild the CTS log wrapper" >&2; exit 1; }
elif command -v aarch64-linux-musl-gcc >/dev/null 2>&1; then
    aarch64-linux-musl-gcc -O2 \
        "$ROOT/tests/cts/glcts-log-wrapper.c" -o "$BUILD/glcts-log-wrapper" || {
            echo "could not rebuild the CTS log wrapper" >&2; exit 1; }
fi

# Rebuild that marshaller from the current tree first.
#
# $BUILD/guest-gl is written by tests/gl44-cts.sh when the CTS is *compiled*,
# and never again -- so it goes stale the moment the transport gains a command,
# and it is then copied over the production driver below. On 2026-09-06 it was
# 193,448 bytes against a current 387,768, missing about 130 commands: the CTS
# aborted on "unknown opcode 1306" (glGetTransformFeedbackVarying, generated in
# that old build and hand-written now), and every later browser run inherited
# the stale driver until the assets were rebuilt.
#
# Refreshing it costs a few seconds and cannot break the ABI glcts linked
# against: a shared object that gains symbols still satisfies every old one.
#
# The compiler is looked for in the same three places, in the same order, as
# tests/gl44-probe.sh -- it defaulted to PATH alone, where the cross compiler
# is not, so every run took the warning branch below and served the driver
# from whenever the CTS was last compiled. A warning is the wrong shape for
# this: nothing downstream can tell a stale-driver run from a conformance
# result, and a whole afternoon of buffer_storage numbers were measured
# against a driver that predated the fixes they were meant to check. Refuse
# instead, and let GEX_CTS_ALLOW_STALE say otherwise on purpose.
for candidate in \
    ${GEX_CTS_TOOLCHAIN:+$GEX_CTS_TOOLCHAIN/bin/aarch64-linux-musl-gcc} \
    "${GEX_CC:-/home/velzie/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc}" \
    "$(command -v aarch64-linux-musl-gcc 2>/dev/null || true)"
do
    [ -n "$candidate" ] && [ -x "$candidate" ] && { GUEST_CC=$candidate; break; }
done
if [ -n "${GUEST_CC:-}" ]; then
    echo "guest driver: rebuilding with $GUEST_CC" >&2
    GEX_CC="$GUEST_CC" "$ROOT/guest/gl/build.sh" "$BUILD/guest-gl" >/dev/null || {
        echo "could not rebuild the CTS guest driver" >&2; exit 1; }
elif [ -n "${GEX_CTS_ALLOW_STALE:-}" ]; then
    echo "warning: no musl cross compiler; serving whatever guest driver" >&2
    echo "         $BUILD/guest-gl already holds, which may be stale" >&2
else
    echo "no musl cross compiler, so the guest driver cannot be rebuilt and" >&2
    echo "$BUILD/guest-gl would be served as-is. Set GEX_CC or" >&2
    echo "GEX_CTS_TOOLCHAIN, or GEX_CTS_ALLOW_STALE=1 to run anyway." >&2
    exit 2
fi

# glcts links the one-object EGL/GLES marshaller for the few EGL calls the
# surfaceless platform makes directly. Install the exact build it linked.
if [ -f "$BUILD/guest-gl/libGLESv2.so.2" ]; then
    cp "$BUILD/guest-gl/libGLESv2.so.2" "$ROOT/web/public/fs/libGLESv2.so.2"
    cp "$BUILD/guest-gl/libEGL.so.1" "$ROOT/web/public/fs/libEGL.so.1"
    # GEX_CTS_SKIP_BUILD serves the existing production tree, so stage the
    # same driver there as well. Otherwise CTS can silently execute an older
    # ABI than the one this script just selected.
    mkdir -p "$ROOT/web/dist/fs"
    cp "$BUILD/guest-gl/libGLESv2.so.2" "$ROOT/web/dist/fs/libGLESv2.so.2"
    cp "$BUILD/guest-gl/libEGL.so.1" "$ROOT/web/dist/fs/libEGL.so.1"
    # And libGL.so.1, which GLX applications load and which nothing else here
    # refreshes -- leaving a stale one behind broke glmark2 for the same
    # reason this block just broke the CTS.
    [ -f "$BUILD/guest-gl/libGL.so.1" ] && {
        cp "$BUILD/guest-gl/libGL.so.1" "$ROOT/web/public/fs/libGL.so.1"
        cp "$BUILD/guest-gl/libGL.so.1" "$ROOT/web/dist/fs/libGL.so.1"
    }
fi

if [ "${GEX_CTS_SKIP_BUILD:-0}" = 1 ]; then
    # Reuse an already-built frontend while still giving the guest binary a
    # content-addressed URL. This keeps iterative CTS cases to a few seconds.
    mkdir -p "$ROOT/web/dist/fs"
    cp "$CTS_PUBLIC" "$CTS_DIST"
else
    (cd "$ROOT/web" && npm run build)
fi

CLIENT_NAME=$CTS_NAME
EXTRA_CLIENT_ARG=
WANT=DONE!
if [ -x "$WRAPPER" ]; then
    WRAPPER_TAG=$(sha256sum "$WRAPPER" | cut -c1-16)
    WRAPPER_NAME="glcts-log-wrapper-$WRAPPER_TAG"
    WRAPPER_PUBLIC="$ROOT/web/public/fs/$WRAPPER_NAME"
    WRAPPER_DIST="$ROOT/web/dist/fs/$WRAPPER_NAME"
    cp "$WRAPPER" "$WRAPPER_PUBLIC"
    cp "$WRAPPER" "$WRAPPER_DIST"
    CLIENT_NAME=$WRAPPER_NAME
    EXTRA_CLIENT_ARG="--query glarg=--gex-glcts=/fs/$CTS_NAME"
    WANT=QPA-END
fi

# The CTS's own data archive. A ShaderLibraryGroup reads its .test file when
# the group is entered, and a missing one throws tcu::ResourceError out of
# init() -- which dEQP treats as fatal and which ends the run before any case
# executes. KHR-GL32.shaders30 died in five seconds on exactly that. The files
# are served from web/public and land in the guest at the same path, which is
# where glcts looks for them relative to its cwd of "/".
CTS_DATA_SRC=$ROOT/third_party/vk-gl-cts/external/openglcts/data/gl_cts/data
CTS_DATA_ARGS=
if [ -d "$CTS_DATA_SRC" ]; then
    for tree in gl30 common; do
        [ -d "$CTS_DATA_SRC/$tree" ] || continue
        for tree_root in "$ROOT/web/public" "$ROOT/web/dist"; do
            mkdir -p "$tree_root/gl_cts/data"
            cp -r "$CTS_DATA_SRC/$tree" "$tree_root/gl_cts/data/"
        done
        for file in "$CTS_DATA_SRC/$tree"/*; do
            [ -f "$file" ] || continue
            CTS_DATA_ARGS="$CTS_DATA_ARGS --query glfile=/gl_cts/data/$tree/$(basename "$file")"
        done
    done
fi

# Loading the large official runner emits bounded arena-capacity diagnostics
# on stderr. The CTS result matcher remains the pass/fail authority here.
HARNESS_OUTPUT=$(mktemp /tmp/gex-glcts-browser.XXXXXX)
# Extra page flags for a diagnostic run, space separated and without the
# `--query`: GEX_CTS_QUERY='trapstack=1 checksp=1'. Kept out of the default so
# that a measurement and a diagnosis are the same command with one variable
# between them.
EXTRA_QUERY=
for q in ${GEX_CTS_QUERY:-}; do
    # GEX_CTS_QUERY carries page flags; the case filter is GEX_CTS_CASE. Naming
    # the wrong one is a quiet way to measure nothing: the filter reaches the
    # page as a dead URL parameter, the default single case runs, and 1/1
    # passed reads exactly like a family that passed. Refuse instead.
    case "$q" in
        *KHR-GL*|*GTF-GL*|*dEQP*)
            echo "gl44-cts-browser: '$q' is a case name, and GEX_CTS_QUERY is" >&2
            echo "  for page flags -- the case filter is GEX_CTS_CASE. Passing" >&2
            echo "  it here runs the default case only, which looks like a pass." >&2
            exit 2 ;;
    esac
    EXTRA_QUERY="$EXTRA_QUERY --query $q"
done
set +e
GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
node "$ROOT/web/test/drive.mjs" gears \
    --allow-errors \
    --query "$BACKEND_QUERY" \
    --query "glclient=/fs/$CLIENT_NAME" \
    --query "glfile=/fs/$CTS_NAME" \
    $EXTRA_CLIENT_ARG \
    $CTS_DATA_ARGS \
    --query "glarg=--deqp-case=$CASE" \
    --query 'glarg=--deqp-surface-type=pbuffer' \
    --query 'glarg=--deqp-gl-config-name=rgba8888d24s8ms0' \
    --query 'glarg=--deqp-surface-width=64' \
    --query 'glarg=--deqp-surface-height=64' \
    --query "glarg=--deqp-log-filename=$LOG_FILENAME" \
    $EXTRA_QUERY \
    --want "$WANT" \
    --wall "${GEX_CTS_WALL:-300}" --idle "${GEX_CTS_IDLE:-120}" \
    >"$HARNESS_OUTPUT" 2>&1
harness_status=$?
set -e
cat "$HARNESS_OUTPUT"
[ "$harness_status" -eq 0 ] || exit "$harness_status"
# The browser driver only knows whether its requested terminal marker appeared.
# A trapped guest can still print QPA-END from the waiting wrapper, so reject
# process/runtime failures independently of the CTS summary.
if grep -Eq 'Fail \(|Failed:[[:space:]]+[1-9]|Test run was ABORTED|runtime trapped inside the shim|gex: process [0-9]+ failed|exit [1-9][0-9]* in' "$HARNESS_OUTPUT"; then
    exit 1
fi
