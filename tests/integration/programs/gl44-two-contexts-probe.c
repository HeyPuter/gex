/* Two GL contexts, and whether they are two.
 *
 * The guest creates its host GL context behind a latch -- context_create()
 * returns early once host_context_made is set -- so every EGLContext an
 * application creates may alias one host context. If so, a program with two
 * contexts gets two handles to one pile of state: bindings, objects and
 * pixel-store settings all shared, and destroying one destroying nothing.
 *
 * That is worth a test rather than an assertion from reading the code, because
 * the consequence is specific and checkable: state set while one context is
 * current must not be visible from the other. Toolkits that create a second
 * context for a second window, or for loading resources on another thread, are
 * the shape that would hit it.
 *
 * The reference is the oracle here: native Mesa gives real per-context state,
 * so whatever it answers is what GL requires.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-40s got=%lld want=%lld\n", name, got, want);
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
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    EGLContext first = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLContext second = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    if (!s || !first || !second) {
        fprintf(stderr, "INFO could not create two contexts\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 3;
    }
    ++cases; ++passed;
    printf("CASE two_contexts_created PASS\n");

    /* State set in the first, read from the second. */
    if (!eglMakeCurrent(d, s, s, first)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    GLuint buf = 0; glGenBuffers(1, &buf);
    glBindBuffer(GL_ARRAY_BUFFER, buf);
    glBufferData(GL_ARRAY_BUFFER, 64, NULL, GL_STATIC_DRAW);
    eq("first_context_setup_is_clean", drain(), GL_NO_ERROR);

    if (!eglMakeCurrent(d, s, s, second)) return 3;
    GLint value = 0;
    glGetIntegerv(GL_PACK_ALIGNMENT, &value);
    eq("second_context_has_default_pack", value, 4);
    glGetIntegerv(GL_ARRAY_BUFFER_BINDING, &value);
    eq("second_context_has_no_binding", value, 0);

    /* And back: the first context must still have its own. */
    if (!eglMakeCurrent(d, s, s, first)) return 3;
    glGetIntegerv(GL_PACK_ALIGNMENT, &value);
    eq("first_context_kept_its_pack", value, 1);
    glGetIntegerv(GL_ARRAY_BUFFER_BINDING, &value);
    eq("first_context_kept_its_binding", value, (long long)buf);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
