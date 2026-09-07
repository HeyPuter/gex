/* Whether the indexed getters reject an out-of-range index, and whether
 * glBindBufferOffsetEXT exists at all.
 *
 * Found by diffing the *error streams* of one CTS case rather than its result.
 * KHR-GL44.transform_feedback.api_errors_test raises 57 errors natively and 41
 * in the browser, and lining the two up named every difference at once:
 *
 *   glBindBufferOffsetEXT      3 errors natively, none in the browser
 *   glGetIntegeri_v            3 errors natively, none in the browser
 *   glGetBooleani_v            1 error  natively, none in the browser
 *
 * A missing error is not cosmetic here: glGetError pops *one* error, so a call
 * that owes one and raises none shifts every later check in the test onto the
 * wrong error, and the failure then surfaces somewhere innocent. That is how
 * this case ended up reported against an invalid-mode check forty calls later.
 *
 * The suspicion for the getters is that the guest answers them locally -- it
 * has to know their sizes anyway -- and a query answered without asking the
 * host cannot raise the host's error. See the note about a helper query
 * leaving an error behind.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;
static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-36s got=0x%04llx want=0x%04llx\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }
static GLenum sole(void) { GLenum e = glGetError(); drain(); return e; }

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

    /* One past the implementation's own maximum, asked for rather than
       assumed: the limit differs between the two paths and a hard-coded index
       would test a different rule on each. */
    GLint max_tf_buffers = 0;
    glGetIntegerv(GL_MAX_TRANSFORM_FEEDBACK_SEPARATE_ATTRIBS, &max_tf_buffers);
    drain();
    const GLuint over = (GLuint)max_tf_buffers;
    fprintf(stderr, "INFO max_separate_attribs=%d, asking for index=%u\n",
            max_tf_buffers, over);

    GLint iv = 0;
    glGetIntegeri_v(GL_TRANSFORM_FEEDBACK_BUFFER_BINDING, over, &iv);
    eq("get_integeri_v_over_range", sole(), GL_INVALID_VALUE);

    GLboolean bv = GL_FALSE;
    glGetBooleani_v(GL_TRANSFORM_FEEDBACK_BUFFER_BINDING, over, &bv);
    eq("get_booleani_v_over_range", sole(), GL_INVALID_VALUE);

    GLint64 i64 = 0;
    glGetInteger64i_v(GL_TRANSFORM_FEEDBACK_BUFFER_START, over, &i64);
    eq("get_integer64i_v_over_range", sole(), GL_INVALID_VALUE);

    /* The same rule on a different indexed limit, so a fix that special-cases
       one enum does not read as general. */
    GLint max_uniform_buffers = 0;
    glGetIntegerv(GL_MAX_UNIFORM_BUFFER_BINDINGS, &max_uniform_buffers);
    drain();
    glGetIntegeri_v(GL_UNIFORM_BUFFER_BINDING, (GLuint)max_uniform_buffers, &iv);
    eq("get_integeri_v_ubo_over_range", sole(), GL_INVALID_VALUE);

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
