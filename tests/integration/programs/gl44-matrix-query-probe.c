/* The compatibility profile's multi-valued state, and the matrices above all.
 *
 * The guest and the host each carry a table of how many values a glGet pname
 * returns, and they have to agree exactly: the guest sizes the reply it will
 * accept from its copy, so a disagreement makes it drop the answer whole.
 * They agreed -- on one -- for every pname neither had, and neither had any
 * of the compatibility profile's. glGetFloatv(GL_MODELVIEW_MATRIX, m)
 * therefore carried a single float and left the caller's other fifteen
 * holding whatever they held before, with no GL error raised.
 *
 * A GLX context is a compatibility context, and fixed-function software reads
 * its matrices back constantly -- to compose a transform, to unproject a
 * mouse click, to save and restore state around a helper. This probe asks for
 * a compatibility profile explicitly, because an EGL context defaults to core
 * in the browser and fixed function is not reachable there at all.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <stdio.h>
#include <math.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static int near_enough(float a, float b) { return fabsf(a - b) < 0.001f; }

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) { fprintf(stderr, "INFO no config\n"); return 3; }
    /* Compatibility, named explicitly: 0x30FD is
     * EGL_CONTEXT_OPENGL_PROFILE_MASK and bit 1 is the compatibility bit. */
    const EGLint cattrs[] = { 0x3098, 3, 0x30FB, 0, 0x30FD, 0x2, EGL_NONE };
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, cattrs);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) {
        fprintf(stderr, "INFO no compatibility context\n"); return 3;
    }
    const char *version = (const char *)glGetString(GL_VERSION);
    fprintf(stderr, "INFO version=%s\n", version ? version : "(null)");
    /* Fixed function only exists in a compatibility profile. Skipping is the
     * honest answer where it does not; reporting cases=0 is not. */
    GLint profile = 0;
    glGetIntegerv(0x9126 /* GL_CONTEXT_PROFILE_MASK */, &profile);
    while (glGetError() != GL_NO_ERROR) {}
    if (!(profile & 0x2)) {
        fprintf(stderr, "INFO profile mask 0x%x is not compatibility\n",
                (unsigned)profile);
        return 3;
    }

    /* A matrix with sixteen distinct values, so a slot that was not written
     * cannot coincidentally hold the right one. */
    GLfloat wanted[16];
    for (int i = 0; i < 16; ++i) wanted[i] = (GLfloat)(i + 1);
    glMatrixMode(GL_MODELVIEW);
    glLoadMatrixf(wanted);

    GLfloat got[16];
    for (int i = 0; i < 16; ++i) got[i] = -1.0f;
    glGetFloatv(GL_MODELVIEW_MATRIX, got);
    const GLenum error = glGetError();
    int right = 0;
    for (int i = 0; i < 16; ++i) if (near_enough(got[i], wanted[i])) ++right;
    fprintf(stderr, "INFO modelview got=%g,%g,%g..%g correct=%d/16 "
            "err=0x%04x\n", got[0], got[1], got[2], got[15], right,
            (unsigned)error);
    result("modelview_matrix", error == GL_NO_ERROR && right == 16);

    /* And the projection matrix, which is a different stack. */
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    GLfloat proj[16];
    for (int i = 0; i < 16; ++i) proj[i] = -1.0f;
    glGetFloatv(GL_PROJECTION_MATRIX, proj);
    int identity = 1;
    for (int i = 0; i < 16; ++i) {
        const float expect = (i % 5 == 0) ? 1.0f : 0.0f;
        if (!near_enough(proj[i], expect)) identity = 0;
    }
    fprintf(stderr, "INFO projection identity=%d first=%g last=%g\n",
            identity, proj[0], proj[15]);
    result("projection_matrix", identity);

    /* Four-component current state, the same shape one size down. */
    glColor4f(0.25f, 0.5f, 0.75f, 1.0f);
    GLfloat colour[4] = { -1.0f, -1.0f, -1.0f, -1.0f };
    glGetFloatv(GL_CURRENT_COLOR, colour);
    fprintf(stderr, "INFO current_color=%g,%g,%g,%g want=0.25,0.5,0.75,1\n",
            colour[0], colour[1], colour[2], colour[3]);
    result("current_color", near_enough(colour[0], 0.25f) &&
                            near_enough(colour[1], 0.5f) &&
                            near_enough(colour[2], 0.75f) &&
                            near_enough(colour[3], 1.0f));

    /* Three components. */
    glNormal3f(0.0f, 1.0f, 0.0f);
    GLfloat normal[3] = { -1.0f, -1.0f, -1.0f };
    glGetFloatv(GL_CURRENT_NORMAL, normal);
    fprintf(stderr, "INFO current_normal=%g,%g,%g want=0,1,0\n",
            normal[0], normal[1], normal[2]);
    result("current_normal", near_enough(normal[0], 0.0f) &&
                             near_enough(normal[1], 1.0f) &&
                             near_enough(normal[2], 0.0f));

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
