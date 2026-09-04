#!/bin/bash
# Run native CTS cases against gex_wgpu, or against llvmpipe with GEX_REF=1.
#
# The reference run is not optional. A CTS family's score means nothing on its
# own: shading_language_420pack scored 47/329 here and 329/329 on llvmpipe,
# which is what said the failures were real, while other families' failures
# turned out to be the harness. Always have the reference number before
# reading anything into a driver number.
#
#   tests/gl-cts-native.sh --deqp-case='KHR-GL45.cull_distance.*'
#   GEX_REF=1 tests/gl-cts-native.sh --deqp-case='KHR-GL45.cull_distance.*'
#
# The case list lives in the CTS build's own mustpass file:
#   gl_cts/data/mustpass/gl/khronos_mustpass/main/gl45-main.txt
# (19714 cases; the `khronos_mustpass_single` list is a different, much
# smaller set and does not contain most families.)
set -u
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
CTSDIR=${GEX_CTSDIR:-/tmp/gex-glcts-native/external/openglcts/modules}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${GEX_DISPLAY:-:78}
# Without this the CTS aborts before running a single case:
#
#     FATAL ERROR: GLX extension "GLX_ARB_create_context_robustness" not
#     supported at tcuLnxX11GlxPlatform.cpp:315
#
# The driver advertises it -- caps.device_reset_status_query is what Mesa gates
# it on -- but Mesa withholds it unless the *X server* advertises it too,
# because it is not marked direct_only in known_glx_extensions, and Xvfb's GLX
# does not. For direct rendering the extension carries no GLX protocol, so
# forcing it client-side is sound rather than papering over a driver gap.
#
# Keep tests/cts-drirc/00-robustness.conf free of XML comments: Mesa's driconf
# parser silently ignores a file that has one, and the only symptom is the
# FATAL ERROR above coming back.
export DRIRC_CONFIGDIR=${DRIRC_CONFIGDIR:-$ROOT/tests/cts-drirc}
cd "$CTSDIR" || { echo "no CTS build at $CTSDIR" >&2; exit 2; }
if [ "${GEX_REF:-0}" = 1 ]; then
    exec env -u LD_LIBRARY_PATH -u LIBGL_DRIVERS_PATH -u GALLIUM_DRIVER \
        LIBGL_ALWAYS_SOFTWARE=1 ./glcts "$@"
fi
export LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib
export LIBGL_DRIVERS_PATH=$INST/lib/dri
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=gex_wgpu
exec ./glcts "$@"
