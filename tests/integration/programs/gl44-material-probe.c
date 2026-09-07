/* glMaterialfv(face, GL_AMBIENT_AND_DIFFUSE, rgba) carries four values.
 *
 * It is how fixed-function content colours a lit object, and the guest's
 * component table gave it three. The host fills a zeroed array with the count
 * it is handed, so the fourth -- alpha -- arrived as zero and every such
 * material was fully transparent in the alpha channel.
 *
 * There is no glGetMaterialfv in the transport to read the state back with
 * (see the note about the absent compatibility getters), so this renders
 * instead: a lit quad under a white light, and the pixel it produces. That is
 * the better test anyway -- it asks what the material did rather than what
 * the driver says it stored.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <stdio.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                          EGL_ALPHA_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    /* Compatibility, named explicitly: fixed-function lighting does not exist
     * in a core profile, and an EGL context defaults to core in the browser. */
    const EGLint cattrs[] = { 0x3098, 3, 0x30FB, 0, 0x30FD, 0x2, EGL_NONE };
    const EGLint sa[] = { EGL_WIDTH, 32, EGL_HEIGHT, 32, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, cattrs);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    GLint profile = 0;
    glGetIntegerv(0x9126 /* GL_CONTEXT_PROFILE_MASK */, &profile);
    while (glGetError() != GL_NO_ERROR) {}
    if (!(profile & 0x2)) {
        fprintf(stderr, "INFO profile 0x%x is not compatibility\n",
                (unsigned)profile);
        return 3;
    }
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    glViewport(0, 0, 32, 32);
    glClearColor(0.0f, 0.0f, 0.0f, 0.0f);
    glClear(GL_COLOR_BUFFER_BIT);

    /* A white light straight down the -z axis, and a material whose ambient
     * and diffuse are a known RGBA with alpha 1. */
    const GLfloat white[4] = { 1.0f, 1.0f, 1.0f, 1.0f };
    const GLfloat position[4] = { 0.0f, 0.0f, 1.0f, 0.0f };
    glEnable(GL_LIGHTING);
    glEnable(GL_LIGHT0);
    glLightfv(GL_LIGHT0, GL_DIFFUSE, white);
    glLightfv(GL_LIGHT0, GL_AMBIENT, white);
    glLightfv(GL_LIGHT0, GL_POSITION, position);
    const GLfloat material[4] = { 1.0f, 0.0f, 0.0f, 1.0f };
    glMaterialfv(GL_FRONT_AND_BACK, GL_AMBIENT_AND_DIFFUSE, material);

    glMatrixMode(GL_PROJECTION); glLoadIdentity();
    glMatrixMode(GL_MODELVIEW);  glLoadIdentity();
    glNormal3f(0.0f, 0.0f, 1.0f);
    glBegin(GL_TRIANGLE_STRIP);
    glVertex3f(-1.0f, -1.0f, 0.0f);
    glVertex3f( 1.0f, -1.0f, 0.0f);
    glVertex3f(-1.0f,  1.0f, 0.0f);
    glVertex3f( 1.0f,  1.0f, 0.0f);
    glEnd();
    glFinish();

    unsigned char px[4] = { 0, 0, 0, 0 };
    glReadPixels(16, 16, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO lit pixel=%u,%u,%u,%u want red with alpha 255 "
            "err=0x%04x\n", px[0], px[1], px[2], px[3], (unsigned)error);
    result("lit_material_colour", error == GL_NO_ERROR && px[0] > 200 &&
                                  px[1] < 60 && px[2] < 60);
    /* The alpha is the fourth component the table used to drop. */
    result("lit_material_alpha", px[3] > 200);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
