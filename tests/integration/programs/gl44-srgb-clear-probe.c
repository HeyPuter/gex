/* GL_FRAMEBUFFER_SRGB and the clear value.
 *
 * With sRGB conversion *disabled* -- which is the default -- a clear writes
 * the value it was given straight into an sRGB attachment, and glReadPixels
 * hands the same bytes back. With it enabled the clear is encoded on the way
 * in, so the same 0.25 comes back as srgb_encode(0.25) = 0.537, and a draw of
 * a linear colour lands encoded too.
 *
 * This exists because the driver had the second behaviour in both cases. The
 * clear built its attachment view with no format, so it defaulted to the
 * texture's own -- and st/mesa expresses "sRGB off" by attaching the sRGB
 * texture through a *linear* surface, which the view then ignored. A clear to
 * 0.25 read back 137 with nothing reporting a problem. The draw path had
 * always named the surface's format; only the clear had not.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

static int failures = 0;
static int cases = 0;

#define CHECK(condition, message) do { \
    if (!(condition)) { \
        fprintf(stderr, "srgb clear failed: %s\n", message); return __LINE__; } \
} while (0)

static void expect(const char *what, unsigned got, unsigned want) {
    ++cases;
    const int slack = 2;
    if ((int)got - (int)want > slack || (int)want - (int)got > slack) {
        ++failures;
        fprintf(stderr, "FAIL %s: red=%u expected %u\n", what, got, want);
    } else {
        fprintf(stderr, "ok   %s: red=%u\n", what, got);
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    CHECK(eglChooseConfig(display, config_attributes, &config, 1, &config_count),
          "eglChooseConfig");
    CHECK(config_count >= 1 && config != NULL, "no pbuffer config");
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    CHECK(context != EGL_NO_CONTEXT, "eglCreateContext");
    CHECK(surface != EGL_NO_SURFACE, "eglCreatePbufferSurface");
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    /* An SRGB8_ALPHA8 renderbuffer, which is what GL_FRAMEBUFFER_SRGB acts
     * on. A renderbuffer rather than a texture because that is the shape the
     * CTS uses and the one that found this. */
    GLuint rb = 0, fbo = 0;
    glGenRenderbuffers(1, &rb);
    glBindRenderbuffer(GL_RENDERBUFFER, rb);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_SRGB8_ALPHA8, 8, 8);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, rb);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "framebuffer completeness");
    glViewport(0, 0, 8, 8);

    const GLfloat quarter[] = {0.25f, 0.0f, 0.0f, 1.0f};
    unsigned char pixel[4] = {0, 0, 0, 0};

    /* 0.25 * 255 = 63.75, so 64. Encoded it would be 137. */
    glDisable(GL_FRAMEBUFFER_SRGB);
    glClearBufferfv(GL_COLOR, 0, quarter);
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    CHECK(glGetError() == GL_NO_ERROR, "clear/readback with sRGB off");
    expect("clear, sRGB off", pixel[0], 64);

    /* Enabled, the same clear is encoded on the way in. GL says the clear
     * colour is converted when GL_FRAMEBUFFER_SRGB is enabled and the
     * attachment has an sRGB encoding. */
    glEnable(GL_FRAMEBUFFER_SRGB);
    glClearBufferfv(GL_COLOR, 0, quarter);
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    CHECK(glGetError() == GL_NO_ERROR, "clear/readback with sRGB on");
    expect("clear, sRGB on", pixel[0], 137);

    /* And back off again, so a stale view cannot pass this by accident. */
    glDisable(GL_FRAMEBUFFER_SRGB);
    const GLfloat half[] = {0.5f, 0.0f, 0.0f, 1.0f};
    glClearBufferfv(GL_COLOR, 0, half);
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    CHECK(glGetError() == GL_NO_ERROR, "second clear with sRGB off");
    expect("clear 0.5, sRGB off", pixel[0], 128);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures,
           failures);
    return 0;
}
