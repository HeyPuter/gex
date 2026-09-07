/* Reading fixed-function state back.
 *
 * The setters have been in the transport since glxgears needed them; the
 * getters never were. A missing entry point is not a wrong answer -- the
 * guest's libGL does not export the symbol, so a program that reads this
 * state fails to link and never starts, which is a failure no rendering test
 * can see. Neither tools/gl44_coverage.py (it counts the *core* command set,
 * and these are compatibility) nor tests/gl-app-sweep.sh (it runs the native
 * libGL, where the symbols exist) can find the gap either.
 *
 * Query, change, put back is the usual reason to read it: a helper that has
 * to leave the state as it found it.
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
static int near_enough(float a, float b) { return fabsf(a - b) < 0.002f; }

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint cattrs[] = { 0x3098, 3, 0x30FB, 0, 0x30FD, 0x2, EGL_NONE };
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
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

    /* Four values, and all four distinct so a short reply is visible. */
    const GLfloat diffuse[4] = { 0.125f, 0.25f, 0.5f, 0.75f };
    glLightfv(GL_LIGHT0, GL_DIFFUSE, diffuse);
    GLfloat got4[4] = { -1.0f, -1.0f, -1.0f, -1.0f };
    glGetLightfv(GL_LIGHT0, GL_DIFFUSE, got4);
    GLenum error = glGetError();
    fprintf(stderr, "INFO light_diffuse=%g,%g,%g,%g want=0.125,0.25,0.5,0.75 "
            "err=0x%04x\n", (double)got4[0], (double)got4[1], (double)got4[2],
            (double)got4[3], (unsigned)error);
    result("light_diffuse", error == GL_NO_ERROR &&
                            near_enough(got4[0], 0.125f) &&
                            near_enough(got4[1], 0.25f) &&
                            near_enough(got4[2], 0.5f) &&
                            near_enough(got4[3], 0.75f));

    /* Three values: the spot direction. */
    const GLfloat spot[3] = { 0.0f, 0.0f, -1.0f };
    glLightfv(GL_LIGHT0, GL_SPOT_DIRECTION, spot);
    GLfloat got3[3] = { -9.0f, -9.0f, -9.0f };
    glGetLightfv(GL_LIGHT0, GL_SPOT_DIRECTION, got3);
    fprintf(stderr, "INFO spot_direction=%g,%g,%g want=0,0,-1\n",
            (double)got3[0], (double)got3[1], (double)got3[2]);
    result("light_spot_direction", near_enough(got3[0], 0.0f) &&
                                   near_enough(got3[1], 0.0f) &&
                                   near_enough(got3[2], -1.0f));

    /* One value. */
    glLightf(GL_LIGHT0, GL_SPOT_CUTOFF, 45.0f);
    GLfloat cutoff = -1.0f;
    glGetLightfv(GL_LIGHT0, GL_SPOT_CUTOFF, &cutoff);
    fprintf(stderr, "INFO spot_cutoff=%g want=45\n", (double)cutoff);
    result("light_spot_cutoff", near_enough(cutoff, 45.0f));

    /* The material property whose count was wrong: four, not three. */
    const GLfloat ambient_diffuse[4] = { 0.1f, 0.2f, 0.3f, 0.4f };
    glMaterialfv(GL_FRONT, GL_AMBIENT_AND_DIFFUSE, ambient_diffuse);
    GLfloat material[4] = { -1.0f, -1.0f, -1.0f, -1.0f };
    glGetMaterialfv(GL_FRONT, GL_DIFFUSE, material);
    fprintf(stderr, "INFO material_diffuse=%g,%g,%g,%g want=0.1,0.2,0.3,0.4\n",
            (double)material[0], (double)material[1], (double)material[2],
            (double)material[3]);
    result("material_diffuse", near_enough(material[0], 0.1f) &&
                               near_enough(material[1], 0.2f) &&
                               near_enough(material[2], 0.3f) &&
                               near_enough(material[3], 0.4f));

    GLfloat shininess = -1.0f;
    glMaterialf(GL_FRONT, GL_SHININESS, 32.0f);
    glGetMaterialfv(GL_FRONT, GL_SHININESS, &shininess);
    fprintf(stderr, "INFO shininess=%g want=32\n", (double)shininess);
    result("material_shininess", near_enough(shininess, 32.0f));

    /* Four doubles, and no pname. */
    const GLdouble plane[4] = { 1.0, 0.0, 0.0, -0.5 };
    glClipPlane(GL_CLIP_PLANE0, plane);
    GLdouble equation[4] = { -9.0, -9.0, -9.0, -9.0 };
    glGetClipPlane(GL_CLIP_PLANE0, equation);
    fprintf(stderr, "INFO clip_plane=%g,%g,%g,%g want=1,0,0,-0.5\n",
            equation[0], equation[1], equation[2], equation[3]);
    result("clip_plane", near_enough((float)equation[0], 1.0f) &&
                         near_enough((float)equation[1], 0.0f) &&
                         near_enough((float)equation[2], 0.0f) &&
                         near_enough((float)equation[3], -0.5f));

    /* Texture environment: four for the colour, one for the mode. */
    const GLfloat env[4] = { 0.2f, 0.4f, 0.6f, 0.8f };
    glTexEnvfv(GL_TEXTURE_ENV, GL_TEXTURE_ENV_COLOR, env);
    GLfloat env_got[4] = { -1.0f, -1.0f, -1.0f, -1.0f };
    glGetTexEnvfv(GL_TEXTURE_ENV, GL_TEXTURE_ENV_COLOR, env_got);
    GLint mode = 0;
    glGetTexEnviv(GL_TEXTURE_ENV, GL_TEXTURE_ENV_MODE, &mode);
    error = glGetError();
    fprintf(stderr, "INFO tex_env=%g,%g,%g,%g want=0.2,0.4,0.6,0.8 mode=0x%x "
            "err=0x%04x\n", (double)env_got[0], (double)env_got[1],
            (double)env_got[2], (double)env_got[3], (unsigned)mode,
            (unsigned)error);
    result("tex_env_color", error == GL_NO_ERROR &&
                            near_enough(env_got[0], 0.2f) &&
                            near_enough(env_got[1], 0.4f) &&
                            near_enough(env_got[2], 0.6f) &&
                            near_enough(env_got[3], 0.8f));
    result("tex_env_mode", mode == GL_MODULATE);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
