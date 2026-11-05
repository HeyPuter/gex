/* Fixed-function state the guest library answers itself (guest/gl/
 * gexgl-shadow.inc): the modelview and projection matrices after each kind of
 * matrix command, the matrix stacks at and past their ends, display lists that
 * change them, glPushAttrib/glPopAttrib, and glIsEnabled. Prints every answer
 * as bits, one line each, so a run under gexc can be compared line for line
 * with a run on native Mesa (test/gl/matrix-shadow).
 *
 *   cc -o probe gl-matrix-shadow-probe.c -lEGL -lGL -lm  (native reference)
 *   gexc run --gl --env LD_LIBRARY_PATH=GUEST_GL probe */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static void show(const char *what) {
    static const GLenum pnames[2] = {GL_MODELVIEW_MATRIX, GL_PROJECTION_MATRIX};
    for (int s = 0; s < 2; s++) {
        float m[16];
        double d[16];
        memset(m, 0, sizeof(m));
        memset(d, 0, sizeof(d));
        glGetFloatv(pnames[s], m);
        glGetDoublev(pnames[s], d);
        printf("%s %s f", what, s ? "projection" : "modelview");
        for (int i = 0; i < 16; i++) {
            uint32_t b;
            memcpy(&b, &m[i], 4);
            printf(" %08x", (unsigned)b);
        }
        printf(" d");
        for (int i = 0; i < 16; i++) {
            float f = (float)d[i];
            uint32_t b;
            memcpy(&b, &f, 4);
            printf(" %08x", (unsigned)b);
        }
        printf("\n");
    }
}

static void enables(const char *what) {
    static const GLenum caps[] = {GL_FOG, GL_BLEND, GL_DEPTH_TEST, GL_CULL_FACE, GL_ALPHA_TEST,
                                  GL_SCISSOR_TEST, GL_LIGHTING, GL_NORMALIZE, GL_DITHER};
    printf("%s enables", what);
    for (unsigned i = 0; i < sizeof(caps) / sizeof(caps[0]); i++) printf(" %d", glIsEnabled(caps[i]));
    printf("\n");
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 16, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE || !eglMakeCurrent(display, surface, surface, context))
        return 2;

    show("initial");
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    glFrustum(-0.5, 0.5, -0.28125, 0.28125, 4.0, 4096.0);
    show("frustum");
    glFrustum(1.0, 1.0, 0.0, 1.0, 1.0, 2.0); /* invalid: no change */
    show("bad frustum");
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();
    glRotatef(-90.0f, 1.0f, 0.0f, 0.0f);
    glRotatef(90.0f, 0.0f, 0.0f, 1.0f);
    show("axis rotates");
    glRotatef(-12.5f, 0.0f, 1.0f, 0.0f);
    glRotatef(33.3f, 0.0f, 0.0f, -1.0f);
    glRotatef(7.0f, -1.0f, 0.0f, 0.0f);
    glRotatef(-71.0f, 0.0f, -1.0f, 0.0f);
    show("signed axis rotates");
    glRotatef(41.0f, 0.3f, -0.7f, 0.2f);
    glRotated(-17.25, 2.0, 1.0, -3.0);
    glRotatef(10.0f, 0.0f, 0.0f, 0.0f);
    glRotatef(0.0f, 1.0f, 2.0f, 3.0f);
    show("general rotates");
    glTranslatef(-123.5f, 456.25f, -64.0f);
    glTranslated(0.1, 0.2, 0.3);
    glScalef(1.5f, -2.0f, 0.25f);
    glScaled(3.0, 1.0, 0.5);
    show("translate scale");
    const float load[16] = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16.5f};
    const double multd[16] = {0.5, 0, 0.1, 0, 0, 2, 0, 0, 0.3, 0, 1, 0, 7, -8, 9, 1};
    glPushMatrix();
    glLoadMatrixf(load);
    glMultMatrixf(load);
    glMultTransposeMatrixd(multd);
    glMultMatrixd(multd);
    show("load mult");
    glLoadTransposeMatrixf(load);
    show("load transpose");
    glPopMatrix();
    show("popped");

    /* The ends of the stacks: overflow and underflow change nothing. */
    for (int i = 0; i < 40; i++) {
        glPushMatrix();
        glTranslatef((float)i, 1.0f, 0.0f);
    }
    show("deep");
    for (int i = 0; i < 45; i++) glPopMatrix();
    show("popped past the bottom");

    /* A mode not kept between: the texture stack, and an ignored mode. */
    glMatrixMode(GL_TEXTURE);
    glLoadIdentity();
    glRotatef(30.0f, 0.0f, 0.0f, 1.0f);
    glMatrixMode(GL_TEXTURE3);
    glMatrixMode(GL_MODELVIEW);
    glTranslatef(1.0f, 2.0f, 3.0f);
    show("after texture");
    glMatrixMode(GL_PROJECTION);
    glOrtho(0.0, 1280.0, 720.0, 0.0, -99999.0, 99999.0);
    glMatrixMode(GL_MODELVIEW);
    show("ortho");

    /* Display lists. */
    GLuint lists = glGenLists(2);
    glNewList(lists, GL_COMPILE);
    glRotatef(45.0f, 0.0f, 1.0f, 0.0f);
    glEnable(GL_FOG);
    glEndList();
    show("compiled");
    enables("compiled");
    glCallList(lists);
    show("called");
    enables("called");
    glNewList(lists + 1, GL_COMPILE_AND_EXECUTE);
    glTranslatef(5.0f, 0.0f, 0.0f);
    glEndList();
    show("compiled and executed");
    glCallList(lists + 1);
    show("called again");

    /* Attribute groups. */
    glMatrixMode(GL_PROJECTION);
    glPushAttrib(GL_TRANSFORM_BIT);
    glMatrixMode(GL_MODELVIEW);
    glPopAttrib();
    glLoadIdentity(); /* the projection, if the mode came back */
    show("transform attrib");
    glMatrixMode(GL_MODELVIEW);
    glDisable(GL_FOG);
    glEnable(GL_BLEND);
    enables("set");
    glPushAttrib(GL_ENABLE_BIT);
    glEnable(GL_FOG);
    glDisable(GL_BLEND);
    glEnable(GL_DEPTH_TEST);
    enables("pushed");
    glPopAttrib();
    enables("popped");
    glEnable(GL_LIGHTING);
    glDisable(GL_DITHER);
    enables("changed");

    eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(display, context);
    printf("DONE\n");
    return 0;
}
