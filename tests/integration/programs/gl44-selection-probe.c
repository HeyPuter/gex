/* GL selection mode, which the guest could not call at all.
 *
 * `glpuzzle` picks its pieces with it, and glSelectBuffer, glRenderMode,
 * glInitNames, glLoadName and glPushName were all missing from the guest's
 * libGL -- so it failed to resolve them and never started.
 *
 * This is the one part of the surface whose result is not pixels: the records
 * are written by GL on the host and read by the application on the guest, so
 * glRenderMode has to bring both its return value and the buffer back. That
 * makes the interesting failure "the right number of hits with the wrong
 * names", which is why the probe pushes two distinct names and checks which
 * one each hit carries rather than just counting them.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint context_attributes[] = {EGL_NONE};
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 8, 8, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 8, 8);

    /* The whole ortho box is the pick region, so anything drawn inside it
     * hits. Two rectangles, one on each side, under two different names. */
    GLuint records[64];
    memset(records, 0xEE, sizeof(records));
    glSelectBuffer(64, records);
    glRenderMode(GL_SELECT);
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    glOrtho(0.0, 8.0, 0.0, 8.0, -1.0, 1.0);
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();
    glInitNames();
    glPushName(11);
    glRectf(0.0f, 0.0f, 3.0f, 8.0f);
    glLoadName(22);
    glRectf(5.0f, 0.0f, 8.0f, 8.0f);
    const GLint hits = glRenderMode(GL_RENDER);
    const GLenum error = glGetError();

    fprintf(stderr, "INFO hits=%d err=0x%04x\n", (int)hits, (unsigned)error);
    result("two_hits", error == GL_NO_ERROR && hits == 2);

    /* Each record is: name count, min depth, max depth, then the names. With
     * one name on the stack that is four words per hit, and the name is the
     * fourth. */
    if (hits == 2) {
        const GLuint first_names = records[0];
        const GLuint first_name = records[3];
        const GLuint second_names = records[4];
        const GLuint second_name = records[7];
        fprintf(stderr, "INFO record0 names=%u name=%u  record1 names=%u "
                "name=%u\n", first_names, first_name, second_names,
                second_name);
        result("record_shape", first_names == 1 && second_names == 1);
        result("names_carried", first_name == 11 && second_name == 22);
    } else {
        result("record_shape", 0);
        result("names_carried", 0);
    }

    /* And nothing outside the pick region hits, which is what says the
     * records came from this frame's geometry rather than from whatever the
     * buffer happened to hold. */
    memset(records, 0xEE, sizeof(records));
    glSelectBuffer(64, records);
    glRenderMode(GL_SELECT);
    glInitNames();
    glPushName(33);
    glRectf(20.0f, 20.0f, 24.0f, 24.0f);
    const GLint misses = glRenderMode(GL_RENDER);
    fprintf(stderr, "INFO outside hits=%d\n", (int)misses);
    result("outside_misses", misses == 0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
