#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "clear buffer failed: %s\n", message); return __LINE__; } \
} while (0)

static void check_pixel(const char* operation, const unsigned char* pixel,
                        int r, int g, int b, int a) {
    const int expected[4] = {r, g, b, a};
    for (int i = 0; i < 4; ++i) {
        if (abs((int)pixel[i] - expected[i]) > 2) {
            fprintf(stderr,
                    "clear buffer failed: %s pixel was %u,%u,%u,%u; expected %d,%d,%d,%d\n",
                    operation, pixel[0], pixel[1], pixel[2], pixel[3], r, g, b, a);
            exit(2);
        }
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    /* An explicit config, and a checked config_count.
     *
     * With a NULL attribute list eglChooseConfig defaults to requiring
     * EGL_SURFACE_TYPE=EGL_WINDOW_BIT, which matches nothing under
     * EGL_PLATFORM=surfaceless: it returned zero configs, `config` stayed
     * NULL, eglCreatePbufferSurface failed with EGL_BAD_CONFIG -- and
     * eglMakeCurrent then *succeeded* anyway, because a surfaceless context
     * may be made current with no surface. That left a context with no
     * default framebuffer, so every clear and readback here returned
     * GL_INVALID_FRAMEBUFFER_OPERATION and this probe could never pass on any
     * driver. The count is checked because a NULL config is not an error
     * until much later. */
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_DEPTH_SIZE, 24, EGL_STENCIL_SIZE, 8, EGL_NONE,
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

    /* Everything below targets an FBO rather than the pbuffer's default
     * framebuffer. Mesa hands a single-buffered pbuffer a GL_BACK draw buffer
     * and a GL_FRONT read buffer, so the clears and the readbacks addressed
     * different buffers and every pixel came back zero. An FBO has one
     * unambiguous attachment, needs no assumptions about front/back, and is
     * what the rest of the probes in this directory already use -- which also
     * makes the guest run comparable to the native one. */
    GLuint color_buffer = 0, depth_stencil_buffer = 0, framebuffer = 0;
    glGenRenderbuffers(1, &color_buffer);
    glBindRenderbuffer(GL_RENDERBUFFER, color_buffer);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 8, 8);
    glGenRenderbuffers(1, &depth_stencil_buffer);
    glBindRenderbuffer(GL_RENDERBUFFER, depth_stencil_buffer);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, 8, 8);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, color_buffer);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT,
                              GL_RENDERBUFFER, depth_stencil_buffer);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "framebuffer completeness");
    glViewport(0, 0, 8, 8);

    unsigned char pixel[4] = {0, 0, 0, 0};
    const GLfloat color[] = {0.25f, 0.5f, 0.75f, 1.0f};
    glClearBufferfv(GL_COLOR, 0, color);
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    CHECK(glGetError() == GL_NO_ERROR, "glClearBufferfv/readback error");
    check_pixel("glClearBufferfv", pixel, 64, 128, 191, 255);

    /* ClearBufferiv and ClearBufferuiv need integer colour buffers.
     *
     * They were being aimed at the RGBA8 attachment above, where the spec
     * makes the result undefined -- the buffer is normalized fixed-point and
     * these entry points write integers. The old expectations (1 -> 255) were
     * guesses at that undefined behaviour; Mesa returns zero. Pointing each
     * clear at a buffer of its own type is what makes the answers defined,
     * and readback of an integer buffer has to ask for GL_RGBA_INTEGER. */
    GLuint signed_buffer = 0, unsigned_buffer = 0;
    glGenRenderbuffers(1, &signed_buffer);
    glBindRenderbuffer(GL_RENDERBUFFER, signed_buffer);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8I, 8, 8);
    glGenRenderbuffers(1, &unsigned_buffer);
    glBindRenderbuffer(GL_RENDERBUFFER, unsigned_buffer);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8UI, 8, 8);

    const GLint signed_color[] = {1, 0, 1, 1};
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, signed_buffer);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "signed integer framebuffer completeness");
    glClearBufferiv(GL_COLOR, 0, signed_color);
    GLint signed_pixel[4] = {9, 9, 9, 9};
    glReadPixels(0, 0, 1, 1, GL_RGBA_INTEGER, GL_INT, signed_pixel);
    CHECK(glGetError() == GL_NO_ERROR, "glClearBufferiv/readback error");
    fprintf(stderr, "INFO clearbufferiv rgba=%d,%d,%d,%d expected=1,0,1,1\n",
            signed_pixel[0], signed_pixel[1], signed_pixel[2], signed_pixel[3]);
    CHECK(signed_pixel[0] == 1 && signed_pixel[1] == 0 &&
          signed_pixel[2] == 1 && signed_pixel[3] == 1,
          "glClearBufferiv value");

    const GLuint unsigned_color[] = {0, 1, 1, 1};
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, unsigned_buffer);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "unsigned integer framebuffer completeness");
    glClearBufferuiv(GL_COLOR, 0, unsigned_color);
    GLuint unsigned_pixel[4] = {9, 9, 9, 9};
    glReadPixels(0, 0, 1, 1, GL_RGBA_INTEGER, GL_UNSIGNED_INT, unsigned_pixel);
    CHECK(glGetError() == GL_NO_ERROR, "glClearBufferuiv/readback error");
    fprintf(stderr, "INFO clearbufferuiv rgba=%u,%u,%u,%u expected=0,1,1,1\n",
            unsigned_pixel[0], unsigned_pixel[1], unsigned_pixel[2],
            unsigned_pixel[3]);
    CHECK(unsigned_pixel[0] == 0 && unsigned_pixel[1] == 1 &&
          unsigned_pixel[2] == 1 && unsigned_pixel[3] == 1,
          "glClearBufferuiv value");

    /* Back to the normalized attachment for the depth/stencil clear. */
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, color_buffer);

    glClearBufferfi(GL_DEPTH_STENCIL, 0, 0.5f, 3);
    CHECK(glGetError() == GL_NO_ERROR, "glClearBufferfi error");
    puts("gl44 clear buffer backend ok");
    return 0;
}
