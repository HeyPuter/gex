/* Framebuffer state queries the guest library answers from its cache
 * (guest/gl/gexgl.c fbq_*): GL_SAMPLES and GL_READ_BUFFER across binding
 * changes, glReadBuffer, renderbuffer storage with samples, and a deleted
 * framebuffer whose name comes back. Prints SUMMARY cases=N passed=N failed=0;
 * runs on native Mesa too.
 *
 *   cc -o probe gl-state-query-probe.c -lEGL -lGL       (native reference)
 *   gexc run --gl --env LD_LIBRARY_PATH=GUEST_GL probe (tests/gl-state-query-cache.sh) */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;
static void expect(const char *name, GLenum pname, GLint want) {
    GLint got = -1;
    glGetIntegerv(pname, &got);
    /* Twice: the second answer is the cached one. */
    GLint again = -2;
    glGetIntegerv(pname, &again);
    int ok = got == want && again == want;
    cases++;
    passed += ok;
    printf("CASE %s %s (got %d then %d, want %d)\n", name, ok ? "PASS" : "FAIL", got, again, want);
}

int main(void) {
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count) || config_count < 1) return 2;
    const EGLint context_attributes[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE};
    const EGLint surface_attributes[] = {EGL_WIDTH, 16, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE || !eglMakeCurrent(display, surface, surface, context))
        return 2;

    GLint default_read = -1;
    glGetIntegerv(GL_READ_BUFFER, &default_read);
    GLuint fbo[2], rb[2];
    glGenFramebuffers(2, fbo);
    glGenRenderbuffers(2, rb);
    glBindRenderbuffer(GL_RENDERBUFFER, rb[0]);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 16, 8);
    glBindRenderbuffer(GL_RENDERBUFFER, rb[1]);
    glRenderbufferStorageMultisample(GL_RENDERBUFFER, 4, GL_RGBA8, 16, 8);
    GLint ms = 0;
    glGetRenderbufferParameteriv(GL_RENDERBUFFER, GL_RENDERBUFFER_SAMPLES, &ms);

    glBindFramebuffer(GL_FRAMEBUFFER, fbo[0]);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[0]);
    expect("fbo0 samples", GL_SAMPLES, 0);
    expect("fbo0 read buffer", GL_READ_BUFFER, GL_COLOR_ATTACHMENT0);
    glReadBuffer(GL_NONE);
    expect("fbo0 read buffer after glReadBuffer", GL_READ_BUFFER, GL_NONE);

    glBindFramebuffer(GL_FRAMEBUFFER, fbo[1]);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[1]);
    expect("fbo1 samples", GL_SAMPLES, ms);
    /* Re-attach the single-sampled one to the same framebuffer. */
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[0]);
    expect("fbo1 samples after re-attaching", GL_SAMPLES, 0);

    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    expect("default read buffer", GL_READ_BUFFER, default_read);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fbo[0]);
    expect("read default, draw fbo0: read buffer", GL_READ_BUFFER, default_read);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo[0]);
    expect("read fbo0: read buffer", GL_READ_BUFFER, GL_NONE);

    /* Deleted and made again: the name may come back, with fresh state. */
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glDeleteFramebuffers(1, &fbo[0]);
    GLuint again = 0;
    glGenFramebuffers(1, &again);
    glBindFramebuffer(GL_FRAMEBUFFER, again);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[0]);
    expect("a new framebuffer: read buffer", GL_READ_BUFFER, GL_COLOR_ATTACHMENT0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
