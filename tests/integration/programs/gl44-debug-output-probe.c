/* GL_ARB_debug_output: errors reach the callback, and glGetError stays exact.
 *
 * The guest answers the debug commands itself (guest/gl/gexgl.c): a callback
 * cannot cross the transport, so each waiting submit carries a GET_ERROR and
 * an error it brings back goes to the callback and to glGetError's slot.
 * wined3d relies on both: with the extension it stops checking every call
 * with glGetError, and its capability probes still call glGetError directly
 * and need the exact answer.
 *
 * Run natively against Mesa for the expected SUMMARY, then in the guest
 * (tests/gl44-probe.sh debug-output). */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "debug output probe: %s\n", message); return __LINE__; } \
} while (0)

static int errors_seen;
static unsigned last_type;

static void APIENTRY on_message(GLenum source, GLenum type, GLuint id,
                                  GLenum severity, GLsizei length,
                                  const GLchar *message, const void *user) {
    (void)source; (void)id; (void)severity; (void)length; (void)message;
    if (user != (const void *)&errors_seen) return;
    if (type == GL_DEBUG_TYPE_ERROR) {
        ++errors_seen;
        last_type = type;
    }
}

static int cases, failed;

static void expect(int ok, const char *what) {
    ++cases;
    if (!ok) {
        ++failed;
        fprintf(stderr, "FAIL %s\n", what);
    } else {
        printf("ok %s\n", what);
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    CHECK(eglChooseConfig(display, config_attributes, &config, 1, &config_count),
          "eglChooseConfig");
    CHECK(config_count >= 1 && config != NULL, "no pbuffer config");
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    CHECK(context != EGL_NO_CONTEXT, "eglCreateContext");
    CHECK(surface != EGL_NO_SURFACE, "eglCreatePbufferSurface");
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    int listed = 0;
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, "GL_ARB_debug_output") == 0) listed = 1;
    }
    expect(listed, "GL_ARB_debug_output is listed");
    while (glGetError() != GL_NO_ERROR) {}

    /* What wined3d does when ERR is on for d3d. */
    glDebugMessageCallback(on_message, &errors_seen);
    glEnable(GL_DEBUG_OUTPUT);
    glDebugMessageControl(GL_DONT_CARE, GL_DONT_CARE, GL_DONT_CARE, 0, NULL, GL_FALSE);
    glDebugMessageControl(GL_DONT_CARE, GL_DEBUG_TYPE_ERROR, GL_DONT_CARE, 0, NULL, GL_TRUE);

    /* 1. An error reaches the callback by the next waiting call. */
    glEnable(0xDEAD);
    glFinish();
    expect(errors_seen >= 1 && last_type == GL_DEBUG_TYPE_ERROR,
           "an invalid enable reaches the callback by glFinish");
    /* 2. ... and is still reported by glGetError, once. */
    GLenum first = glGetError(), second = glGetError();
    expect(first == GL_INVALID_ENUM, "glGetError after the callback is GL_INVALID_ENUM");
    expect(second == GL_NO_ERROR, "and then GL_NO_ERROR");

    /* 3. A probe: an error, then glGetError at once, no waiting call between. */
    glEnable(0xDEAD);
    expect(glGetError() == GL_INVALID_ENUM, "glGetError right after the call is exact");
    expect(glGetError() == GL_NO_ERROR, "and clears");

    /* 4. An error, then a query that waits, then glGetError. */
    glEnable(0xDEAD);
    GLint viewport[4] = {0, 0, 0, 0};
    glGetIntegerv(GL_VIEWPORT, viewport);
    expect(glGetError() == GL_INVALID_ENUM, "an error before a waiting query is kept for glGetError");
    expect(glGetError() == GL_NO_ERROR, "and clears");

    /* 5. No error, no message. */
    int before = errors_seen;
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glFinish();
    expect(errors_seen == before, "valid calls raise no message");
    expect(glGetError() == GL_NO_ERROR, "and no error");

    /* 6. Error messages turned off: none delivered, glGetError unchanged. */
    glDebugMessageControl(GL_DONT_CARE, GL_DEBUG_TYPE_ERROR, GL_DONT_CARE, 0, NULL, GL_FALSE);
    before = errors_seen;
    glEnable(0xDEAD);
    glFinish();
    expect(errors_seen == before, "disabled error messages are not delivered");
    expect(glGetError() == GL_INVALID_ENUM, "but glGetError still reports the error");

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failed, failed);
    eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglTerminate(display);
    return failed != 0;
}
