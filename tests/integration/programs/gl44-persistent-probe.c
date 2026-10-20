/* gl44-persistent-probe: Mesa's immediate-mode vertex buffer (vbo_exec), which
 * with ARB_buffer_storage is a 512 KiB buffer mapped persistent and coherent
 * for the life of the context. gex_wgpu hands such a mapping to the device at
 * every submit, by comparing it with what it last sent and sending the 4 KiB
 * blocks that changed; before that it sent all 512 KiB at every submit (Muck
 * at GL 4.4+: 46 times a frame, whether or not anything was drawn through it).
 * Cases, in a compatibility context, each read back with glReadPixels:
 *   flushed     one glBegin/glEnd quad per column, a glFlush (a submit) after each
 *   wrap        12,000 quads, more vertex data than the buffer holds, flushed
 *               every 1,000 quads; the last quad drawn on each column must win
 *   after_idle  submits that draw nothing through the buffer, then one quad
 * Prints "persist: NAME ok" or "persist: NAME FAIL ..."; the last line is
 * "persist: N failure(s)", and the exit status is 0 only if N is 0. */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

/* Compatibility-profile entry points, which glcorearb.h leaves out. */
extern void glBegin(unsigned);
extern void glEnd(void);
extern void glVertex2f(float, float);
extern void glColor4ub(unsigned char, unsigned char, unsigned char, unsigned char);
extern void glMatrixMode(unsigned);
extern void glLoadIdentity(void);
extern void glOrtho(double, double, double, double, double, double);

#define W 64
#define H 64
#define QUADS 0x0007

static int failures;

static void colour(unsigned k, unsigned char c[3]) {
    c[0] = (unsigned char)(k * 37u + 11u);
    c[1] = (unsigned char)(k * 91u + 40u);
    c[2] = (unsigned char)(k * 13u + 200u);
}

static void quad(unsigned x, unsigned y0, unsigned y1, unsigned k) {
    unsigned char c[3];
    colour(k, c);
    glColor4ub(c[0], c[1], c[2], 255);
    glVertex2f((float)x, (float)y0);
    glVertex2f((float)x + 1, (float)y0);
    glVertex2f((float)x + 1, (float)y1);
    glVertex2f((float)x, (float)y1);
}

/* want[x] is the colour index column x should hold in rows [y0, y1). */
static void check(const char *name, unsigned y0, unsigned y1, const unsigned *want) {
    static unsigned char px[W * H * 4];
    glFinish();
    glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
    for (unsigned y = y0; y < y1; y++)
        for (unsigned x = 0; x < W; x++) {
            unsigned char c[3];
            colour(want[x], c);
            const unsigned char *p = px + (y * W + x) * 4;
            if (p[0] != c[0] || p[1] != c[1] || p[2] != c[2]) {
                printf("persist: %s FAIL pixel %u,%u is %02x%02x%02x, want %02x%02x%02x (quad %u)\n",
                       name, x, y, p[0], p[1], p[2], c[0], c[1], c[2], want[x]);
                failures++;
                return;
            }
        }
    printf("persist: %s ok\n", name);
}

static int fail(const char *what) {
    printf("persist: FAIL %s\npersist: 1 failure(s)\n", what);
    return 1;
}

int main(void) {
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return fail("egl");
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE,
    };
    const EGLint pbuffer_attributes[] = {EGL_WIDTH, W, EGL_HEIGHT, H, EGL_NONE};
    const EGLint context_attributes[] = {
        EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_COMPATIBILITY_PROFILE_BIT, EGL_NONE,
    };
    EGLConfig config = NULL; EGLint count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &count) || count < 1) return fail("config");
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, pbuffer_attributes);
    if (context == EGL_NO_CONTEXT || !eglMakeCurrent(display, surface, surface, context)) return fail("context");
    GLint n = 0, storage = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &n);
    for (GLint i = 0; i < n; i++) {
        const char *e = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (e && !strcmp(e, "GL_ARB_buffer_storage")) storage = 1;
    }
    printf("persist: context %s, ARB_buffer_storage %d\n", (const char *)glGetString(GL_VERSION), storage);

    glViewport(0, 0, W, H);
    glMatrixMode(0x1701 /* GL_PROJECTION */);
    glLoadIdentity();
    glOrtho(0, W, 0, H, -1, 1);
    glMatrixMode(0x1700 /* GL_MODELVIEW */);
    glLoadIdentity();
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);

    unsigned want[W];
    for (unsigned x = 0; x < W; x++) {
        glBegin(QUADS);
        quad(x, 0, 32, x);
        glEnd();
        glFlush();
        want[x] = x;
    }
    check("flushed", 0, 32, want);

    const unsigned total = 12000;
    glBegin(QUADS);
    for (unsigned k = 0; k < total; k++) {
        quad(k % W, 32, H, 100 + k);
        want[k % W] = 100 + k;
        if (k % 1000 == 999) {
            glEnd();
            glFlush();
            glBegin(QUADS);
        }
    }
    glEnd();
    check("wrap", 32, H, want);

    for (int i = 0; i < 8; i++) {
        glScissor(0, 0, 1, 1);
        glFlush();
    }
    glBegin(QUADS);
    quad(0, 0, 32, 7777);
    glEnd();
    glFlush();
    for (unsigned x = 0; x < W; x++) want[x] = x;
    want[0] = 7777;
    check("after_idle", 0, 32, want);

    printf("persist: %d failure(s)\n", failures);
    return failures ? 1 : 0;
}
