/* The compatibility-profile fixed-function calls, one at a time.
 *
 * glxgears needs exactly twenty of these and nothing else, and running it
 * produced a wasm trap inside the host -- `RuntimeError: unreachable` -- part
 * way through building its display lists, which takes the GL context with it
 * and says nothing about which call did it.
 *
 * So each group announces itself before it runs. A trap stops the guest dead,
 * so the last line printed is the group that caused it; on a run that
 * survives, every group also checks glGetError.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

/* glcorearb.h is the core header and omits every one of these, which is the
 * whole reason they had to be added to the transport by hand. */
extern void glMatrixMode(unsigned);
extern void glLoadIdentity(void);
extern void glPushMatrix(void);
extern void glPopMatrix(void);
extern void glTranslatef(float, float, float);
extern void glRotatef(float, float, float, float);
extern void glFrustum(double, double, double, double, double, double);
extern void glBegin(unsigned);
extern void glEnd(void);
extern void glVertex3f(float, float, float);
extern void glNormal3f(float, float, float);
extern void glShadeModel(unsigned);
extern void glLightfv(unsigned, unsigned, const float *);
extern void glMaterialfv(unsigned, unsigned, const float *);
extern unsigned glGenLists(int);
extern void glNewList(unsigned, unsigned);
extern void glEndList(void);
extern void glCallList(unsigned);
extern void glDeleteLists(unsigned, int);

static int cases;
static int passed;

static void step(const char *name) {
    fprintf(stderr, "INFO reached %s\n", name);
}

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static int no_error(const char *name) {
    const GLenum error = glGetError();
    if (error != GL_NO_ERROR)
        fprintf(stderr, "INFO %s err=0x%04x\n", name, (unsigned)error);
    return error == GL_NO_ERROR;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
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
    /* Fixed function exists only in a compatibility profile. */
    const EGLint context_attributes[] = {
        0x30FD /* EGL_CONTEXT_OPENGL_PROFILE_MASK */, 0x2, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    while (glGetError() != GL_NO_ERROR) { }

    step("matrix");
    glMatrixMode(0x1701 /* GL_PROJECTION */);
    glLoadIdentity();
    glFrustum(-1.0, 1.0, -1.0, 1.0, 5.0, 60.0);
    glMatrixMode(0x1700 /* GL_MODELVIEW */);
    glLoadIdentity();
    glPushMatrix();
    glTranslatef(0.0f, 0.0f, -40.0f);
    glRotatef(20.0f, 1.0f, 0.0f, 0.0f);
    glPopMatrix();
    result("matrix", no_error("matrix"));

    step("lighting");
    static const float position[4] = {5.0f, 5.0f, 10.0f, 0.0f};
    static const float colour[4] = {0.8f, 0.1f, 0.0f, 1.0f};
    glLightfv(0x4000 /* GL_LIGHT0 */, 0x1203 /* GL_POSITION */, position);
    glMaterialfv(0x0404 /* GL_FRONT */, 0x1602 /* AMBIENT_AND_DIFFUSE */,
                 colour);
    glShadeModel(0x1D01 /* GL_SMOOTH */);
    glEnable(0x0B50 /* GL_LIGHTING */);
    glEnable(0x4000 /* GL_LIGHT0 */);
    glEnable(0x0B44 /* GL_CULL_FACE */);
    glEnable(0x0B71 /* GL_DEPTH_TEST */);
    result("lighting", no_error("lighting"));

    step("immediate");
    glBegin(0x0004 /* GL_TRIANGLES */);
    glNormal3f(0.0f, 0.0f, 1.0f);
    glVertex3f(-0.5f, -0.5f, 0.0f);
    glVertex3f(0.5f, -0.5f, 0.0f);
    glVertex3f(0.0f, 0.5f, 0.0f);
    glEnd();
    result("immediate", no_error("immediate"));

    step("gen_lists");
    const unsigned list = glGenLists(1);
    fprintf(stderr, "INFO list=%u\n", list);
    result("gen_lists", list != 0 && no_error("gen_lists"));

    step("new_list");
    glNewList(list, 0x1300 /* GL_COMPILE */);
    glBegin(0x0004);
    glNormal3f(0.0f, 0.0f, 1.0f);
    glVertex3f(-0.5f, -0.5f, 0.0f);
    glVertex3f(0.5f, -0.5f, 0.0f);
    glVertex3f(0.0f, 0.5f, 0.0f);
    glEnd();
    glEndList();
    result("new_list", no_error("new_list"));

    step("call_list");
    glClear(0x00004000 /* GL_COLOR_BUFFER_BIT */);
    glCallList(list);
    result("call_list", no_error("call_list"));

    step("delete_lists");
    glDeleteLists(list, 1);
    result("delete_lists", no_error("delete_lists"));

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
