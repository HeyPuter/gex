/* glGetInternalformati64v with a negative bufSize overruns Mesa's stack.
 *
 * _mesa_GetInternalformati64v seeds a 16-element GLint array before calling
 * the 32-bit query that validates its arguments:
 *
 *     GLsizei realSize = MIN2(bufSize, 16);
 *     unsigned i;
 *     ...
 *     for (i = 0; i < realSize; i++)
 *         params32[i] = -1;
 *
 * `i` is unsigned and realSize is signed, so a bufSize of -1 makes realSize
 * -1, which the comparison promotes to about four billion. The seeding then
 * runs off the end of params32 and the process dies -- before
 * _mesa_GetInternalformativ, which does raise GL_INVALID_VALUE for a negative
 * bufSize, is ever reached.
 *
 * This matters here more than it does to an ordinary GL client. The WebGPU
 * host decodes a command stream produced by a sandboxed guest, so a guest
 * could reach a stack overflow in the host by asking for it. The driver's own
 * transport used to stop this by accident, refusing the whole batch as
 * malformed; that refusal was itself a defect -- it destroyed every unrelated
 * command queued behind it -- and removing it exposed this.
 *
 * The stock system Mesa still has the bug, so this probe has no usable native
 * reference: the reference run segfaults and produces no SUMMARY at all. Gate
 * it with GEX_PROBE_EXPECT, the mechanism gl44-probe.sh already documents for
 * probes whose reference is the thing that fails:
 *
 *   GEX_PROBE_EXPECT='SUMMARY cases=2 passed=2 failed=0' \
 *       tests/gl44-probe.sh internalformat-negative
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static GLenum drain(void) {
    GLenum last = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) last = e;
    return last;
}

static void record(const char *name, int ok, const char *detail) {
    ++cases; if (ok) ++passed;
    fprintf(stderr, "INFO %-34s %s\n", name, detail);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    drain();

    /* Surviving the call at all is the first case. A canary either side of the
       destination catches a smaller overrun that does not happen to fault. */
    struct { GLint64 before[2]; GLint64 values[4]; GLint64 after[2]; } box = {
        { -424242, -424242 }, { 0, 0, 0, 0 }, { -424242, -424242 }
    };
    glGetInternalformati64v(GL_TEXTURE_2D, GL_RGBA8, GL_SAMPLES, -1, box.values);
    const GLenum error = drain();
    char detail[128];
    snprintf(detail, sizeof detail, "returned; error=0x%04x", (unsigned)error);
    record("negative_bufsize_returns", error != GL_NO_ERROR, detail);

    const int intact = box.before[0] == -424242 && box.before[1] == -424242 &&
                       box.after[0] == -424242 && box.after[1] == -424242;
    snprintf(detail, sizeof detail, "canaries %s", intact ? "intact" : "OVERWRITTEN");
    record("negative_bufsize_writes_nothing", intact, detail);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
