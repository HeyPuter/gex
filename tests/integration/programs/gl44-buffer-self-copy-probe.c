/* glCopyBufferSubData with one buffer as both source and destination.
 *
 * GL allows it as long as the ranges do not overlap, and says so outright; a
 * program compacting or rotating a buffer in place does exactly this. WebGPU
 * refuses it whatever the ranges --
 *
 *   Source and destination are the same buffer ([Buffer (unlabeled)]).
 *
 * -- and an invalid copy invalidates the command buffer, which fails the
 * submit, which latched the whole context. So it is not one lost copy: every
 * later draw in that context was lost too, silently. It goes through a
 * staging buffer now.
 *
 *   `disjoint`   copy the first half over the second, which GL allows
 *   `survives`   the context still works afterwards -- a plain buffer
 *                readback, which declines outright on a latched context
 *
 * `disjoint` is the one that catches this: measured against the driver with
 * the staging fix removed, it fails and `survives` still passes. So the
 * second case is a guard rather than a reproducer -- this program never
 * submits enough work after the bad copy to feel the latch, while the CTS,
 * which reuses one context for hundreds of cases, felt nothing else.
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

#define WORDS 64

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3,
        EGL_CONTEXT_OPENGL_PROFILE_MASK,
        EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    uint32_t source[WORDS];
    for (unsigned i = 0; i < WORDS; ++i)
        source[i] = 0x1000u + i;

    GLuint buffer = 0;
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(source), source, GL_DYNAMIC_DRAW);

    /* The first half over the second: disjoint, and legal. Bound to two
     * different targets because that is how GL names the two ends, but it is
     * one buffer either way -- which is the whole point. */
    glBindBuffer(GL_COPY_READ_BUFFER, buffer);
    glBindBuffer(GL_COPY_WRITE_BUFFER, buffer);
    glCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER,
                        0, (WORDS / 2) * sizeof(uint32_t),
                        (WORDS / 2) * sizeof(uint32_t));
    const GLenum copy_error = glGetError();

    uint32_t back[WORDS];
    memset(back, 0xAB, sizeof(back));
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glGetBufferSubData(GL_ARRAY_BUFFER, 0, sizeof(back), back);
    const GLenum read_error = glGetError();

    unsigned wrong = 0;
    for (unsigned i = 0; i < WORDS / 2; ++i) {
        if (back[i] != source[i]) ++wrong;                 /* untouched */
        if (back[WORDS / 2 + i] != source[i]) ++wrong;     /* the copy */
    }
    fprintf(stderr, "INFO disjoint wrong=%u first=%08x mid=%08x "
            "copy_err=0x%04x read_err=0x%04x\n", wrong, back[0],
            back[WORDS / 2], (unsigned)copy_error, (unsigned)read_error);
    result("disjoint", copy_error == GL_NO_ERROR && read_error == GL_NO_ERROR &&
                       wrong == 0);

    /* The context still works. On a latched context every later call declines,
     * so this readback of a *different* buffer is what tells a lost copy from
     * a lost context. */
    GLuint other = 0;
    const uint32_t marker[4] = {0xFEEDu, 0xBEEFu, 0xCAFEu, 0xF00Du};
    uint32_t marker_back[4] = {0};
    glGenBuffers(1, &other);
    glBindBuffer(GL_ARRAY_BUFFER, other);
    glBufferData(GL_ARRAY_BUFFER, sizeof(marker), marker, GL_STATIC_DRAW);
    glGetBufferSubData(GL_ARRAY_BUFFER, 0, sizeof(marker_back), marker_back);
    const GLenum after_error = glGetError();
    fprintf(stderr, "INFO survives got=%08x,%08x err=0x%04x\n",
            marker_back[0], marker_back[3], (unsigned)after_error);
    result("survives", after_error == GL_NO_ERROR &&
                       !memcmp(marker, marker_back, sizeof(marker)));

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
