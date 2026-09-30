#!/bin/sh
# Structural gates for the guest's OpenGL 4.4 transport.
#
# This is intentionally not called a conformance test. It proves that every
# core command is represented in the pinned Khronos model and that the guest
# library's coverage never falls below the last reviewed baseline. Semantic
# conformance is covered by separate execution tests as command families land.
set -eu

ROOT="$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-gl44-layer"
DRIVER="$WORK/driver"

mkdir -p "$WORK"

if command -v strat >/dev/null 2>&1 \
    && strat debian test -x /usr/bin/aarch64-linux-gnu-gcc; then
    strat debian env GEX_CC=/usr/bin/aarch64-linux-gnu-gcc \
        "$ROOT/guest/gl/build.sh" "$DRIVER" >/dev/null
elif [ -x "$HOME/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc" ]; then
    GEX_CC="$HOME/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc" \
        "$ROOT/guest/gl/build.sh" "$DRIVER" >/dev/null
else
    echo "skip: no aarch64 C compiler for the guest GL library"
    exit 0
fi

# Khronos' loader resolves the complete EGL 1.5 core table even when a GL-only
# case exercises only a handful of calls. Keep ordinary dynamically linked apps
# working by requiring that ABI at the guest-library boundary.
EGL_EXPORTS=$(nm -D --defined-only "$DRIVER/libEGL.so.1" | awk '{print $3}')
for symbol in \
    eglBindAPI eglBindTexImage eglChooseConfig eglClientWaitSync eglCopyBuffers \
    eglCreateContext eglCreateImage eglCreatePbufferFromClientBuffer \
    eglCreatePbufferSurface eglCreatePixmapSurface eglCreatePlatformPixmapSurface \
    eglCreatePlatformWindowSurface eglCreateSync eglCreateWindowSurface \
    eglDestroyContext eglDestroyImage eglDestroySurface eglDestroySync \
    eglGetConfigAttrib eglGetConfigs eglGetCurrentContext eglGetCurrentDisplay \
    eglGetCurrentSurface eglGetDisplay eglGetError eglGetPlatformDisplay \
    eglGetProcAddress eglGetSyncAttrib eglInitialize eglMakeCurrent eglQueryAPI \
    eglQueryContext eglQueryString eglQuerySurface eglReleaseTexImage \
    eglReleaseThread eglSurfaceAttrib eglSwapBuffers eglSwapInterval eglTerminate \
    eglWaitClient eglWaitGL eglWaitNative eglWaitSync; do
    printf '%s\n' "$EGL_EXPORTS" | grep -qx "$symbol" || {
        echo "missing EGL core export: $symbol" >&2
        exit 1
    }
done

# A null proc lookup must stay null.  CTS keeps a very large dispatch table;
# letting an absent entry alias an unrelated PLT slot turns ordinary coverage
# gaps into misleading calls into libc.
for symbol in \
    glBeginTransformFeedback glGetTexImage glTransformFeedbackVaryings \
    glGenQueries glDeleteQueries glIsQuery \
    glGenTransformFeedbacks glDeleteTransformFeedbacks \
    glBindTransformFeedback glIsTransformFeedback \
    glPauseTransformFeedback glResumeTransformFeedback \
    glGetDoublev glPixelStoref glMultiDrawArrays \
    glPointParameterfv glPointParameteriv \
    glDrawElementsBaseVertex glDrawElementsInstancedBaseInstance \
    glDrawElementsInstancedBaseVertex \
    glDrawElementsInstancedBaseVertexBaseInstance glDrawRangeElements \
    glDrawRangeElementsBaseVertex glMultiDrawElements \
    glMultiDrawElementsBaseVertex glBindSamplers \
    glCompressedTexImage1D glCompressedTexSubImage1D \
    glGetCompressedTexImage glGetInternalformativ glGetMultisamplefv \
    glSamplerParameterIiv glSamplerParameterIuiv \
    glTexParameterIiv glTexParameterIuiv; do
    printf '%s\n' "$EGL_EXPORTS" | grep -qx "$symbol" || {
        echo "missing required GL export: $symbol" >&2
        exit 1
    }
done

python3 "$ROOT/tools/gen_gl44_manifest.py" --output "$WORK/gl44.json"
python3 "$ROOT/tools/gen_gl44_transport.py" --check
python3 - "$WORK/gl44.json" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1]))
assert manifest["api"] == "OpenGL"
assert manifest["profile"] == "core"
assert manifest["version"] == "4.4"
assert len(manifest["commands"]) == 543
assert len({command["name"] for command in manifest["commands"]}) == 543
PY

python3 "$ROOT/tools/gl44_coverage.py" \
    --guest "$DRIVER/libGLESv2.so.2" \
    --min-guest 515

# Resource query replies must be length-delimited and copied only after the
# generated guest verifies the exact pname-dependent output size. A raw host
# pointer or unconditional four-component memcpy would corrupt scalar callers.
python3 - "$ROOT/guest/gl/gl44-generated.inc" "$ROOT/web/mesa/gl44-generated-cases.inc" <<'PY'
import pathlib, sys
guest = pathlib.Path(sys.argv[1]).read_text()
host = pathlib.Path(sys.argv[2]).read_text()
queries = (
    "glGetBooleanv", "glGetDoublev", "glGetFloatv",
    "glGetBufferParameteri64v", "glGetBufferParameteriv",
    "glGetFramebufferAttachmentParameteriv", "glGetRenderbufferParameteriv",
    "glGetSamplerParameterIiv", "glGetSamplerParameterIuiv",
    "glGetSamplerParameterfv", "glGetSamplerParameteriv",
    "glGetTexLevelParameterfv", "glGetTexLevelParameteriv",
    "glGetTexParameterIiv", "glGetTexParameterIuiv",
    "glGetTexParameterfv", "glGetTexParameteriv",
    "glGetVertexAttribIiv", "glGetVertexAttribIuiv",
    "glGetVertexAttribfv", "glGetVertexAttribiv",
)
for name in queries:
    body = guest.split(f"void {name}(", 1)[1].split("\n}", 1)[0]
    assert "reply_blob(&reply_len)" in body, name
    assert "reply_len == expected" in body, name
assert "GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME" in host
assert "GL_VERTEX_ATTRIB_ARRAY_BUFFER_BINDING" in host
PY

# The sync family is deliberately local to the ordered browser transport, but
# it still has to obey the observable GL object lifecycle and query enums.
if command -v cc >/dev/null 2>&1; then
    HOST_DRIVER="$WORK/host-driver"
    GEX_CC="$(command -v cc)" "$ROOT/guest/gl/build.sh" "$HOST_DRIVER" >/dev/null
    cc -O2 -Wall -Wextra -o "$WORK/gl-sync-probe" \
        "$ROOT/tests/integration/programs/gl-sync-probe.c" -ldl
    "$WORK/gl-sync-probe" "$HOST_DRIVER/libGLESv2.so.2"
    cc -O2 -Wall -Wextra -o "$WORK/gl-proc-probe" \
        "$ROOT/tests/integration/programs/gl-proc-probe.c" -ldl
    LD_LIBRARY_PATH="$HOST_DRIVER${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        "$WORK/gl-proc-probe"
fi

# Reader methods mutate the packet cursor. Calling more than one as separate
# C++ arguments makes decoding depend on the compiler's argument order.
if grep -Eq 'gl[A-Za-z0-9_]+\([^;]*a\.(u32|i32|f32|u64|i64|f64)\([^)]*\)[^;]*a\.(u32|i32|f32|u64|i64|f64)\(' \
    "$ROOT/web/mesa/gexgl-mesa.cpp"; then
    echo "unsafe host decode: assign Reader results before making a GL call" >&2
    exit 1
fi
