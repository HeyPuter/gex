/* A display list holding a primitive with too few vertices to draw anything,
 * next to one that does.
 *
 * `dlist-degenerate` draws a white square everywhere else and nothing at all
 * here, and the driver log shows a depth clear and *no draws*: the geometry
 * never reaches the driver, so nothing in the driver can be at fault. What
 * differs is Mesa's own compile path. vbo_save_api.c converts a display
 * list's primitives against ctx->Const.DriverSupportedPrimMask, and the mask
 * comes from the driver -- llvmpipe advertises every mode and so never
 * converts, while this driver advertises five and converts QUADS,
 * TRIANGLE_FAN, POLYGON, LINE_LOOP and QUAD_STRIP on the way in.
 *
 * A primitive with fewer vertices than its mode needs converts to *zero*
 * vertices, and the question this asks is whether that zero is contained or
 * whether it takes the rest of the list with it.
 *
 * Each case compiles one degenerate primitive and one real one into a single
 * list and then asks only about the real one. The real primitive is a
 * TRIANGLE_STRIP quad in a corner of the viewport, in a colour per case, so a
 * case that draws nothing is told apart from one that draws the wrong thing.
 *
 * Both orders are tested. vbo_save merges consecutive primitives of the same
 * mode into one, so "degenerate first" and "degenerate last" reach different
 * code, and a zero-length primitive in the middle of a merge is a different
 * question again.
 *
 * llvmpipe cannot fail this -- it never converts -- so it is a reference for
 * what the list should draw, not for the path under test. radeonsi is the
 * driver to compare against: it advertises no QUADS either, so it takes the
 * same conversion, on real hardware.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 64

/* A twelfth of the target: comfortably less than the quad and comfortably
 * more than nothing. */
static unsigned area_guard(int side) { return (unsigned)side * side / 12; }

/* The real primitive: a quad covering the lower-left quarter, drawn as a
 * strip, which this driver supports natively and so never converts. */
static void real_quad(float r, float g, float b) {
    glColor3f(r, g, b);
    glBegin(GL_TRIANGLE_STRIP);
    glVertex2f(-0.9f, -0.9f);
    glVertex2f(-0.1f, -0.9f);
    glVertex2f(-0.9f, -0.1f);
    glVertex2f(-0.1f, -0.1f);
    glEnd();
}

/* Degenerate: fewer vertices than the mode can make a primitive from, so it
 * contributes nothing however it is converted. */
static void degenerate(GLenum mode, int verts) {
    glBegin(mode);
    for (int i = 0; i < verts; ++i)
        glVertex2f(0.5f + 0.1f * (float)i, 0.5f);
    glEnd();
}

struct probe_case {
    const char *name;
    GLenum mode;
    int verts;
    int degenerate_first;
};

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
    const EGLint surface_attributes[] = {
        EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE,
    };
    /* No version asked for: a compatibility profile, which is the only place
     * display lists and GL_QUADS exist. */
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    glViewport(0, 0, SIDE, SIDE);

    /* Every mode this driver does not advertise, so every mode vbo_save
     * converts. QUADS needs four vertices, TRIANGLE_FAN and POLYGON three,
     * LINE_LOOP two, QUAD_STRIP four. */
    const struct probe_case tests[] = {
        {"quads_2_first",       GL_QUADS,          2, 1},
        {"quads_2_last",        GL_QUADS,          2, 0},
        {"quads_0_first",       GL_QUADS,          0, 1},
        {"trifan_2_first",      GL_TRIANGLE_FAN,   2, 1},
        {"trifan_2_last",       GL_TRIANGLE_FAN,   2, 0},
        {"polygon_2_first",     GL_POLYGON,        2, 1},
        {"lineloop_1_first",    GL_LINE_LOOP,      1, 1},
        {"quadstrip_3_first",   GL_QUAD_STRIP,     3, 1},
        {"quadstrip_2_last",    GL_QUAD_STRIP,     2, 0},
    };

    unsigned char *image = malloc((size_t)SIDE * SIDE * 4);
    if (!image) return 2;

    /* The shape the demo actually has: a list whose *only* primitive converts
     * to zero vertices, so the list records no vertices and no indices at all
     * and vbo_save leaves save->current_bo at NULL. That is the case the two
     * ib.obj guards in vbo_save_api.c were added for -- they stopped a null
     * dereference inside glEndList, which is not the same as the list being
     * harmless afterwards. What matters is whether drawing *after* such a
     * list still works, so the real quad is drawn outside the list here
     * rather than inside it.
     *
     * dlist-degenerate compiles exactly one primitive, GL_POLYGON with two
     * vertices, which is why nothing in the earlier cases reproduced it. */
    {
        GLuint list = glGenLists(1);
        glNewList(list, GL_COMPILE);
        degenerate(GL_POLYGON, 2);
        glEndList();

        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glCallList(list);
        const GLenum after_list = glGetError();
        real_quad(0.0f, 1.0f, 0.0f);
        glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);

        const int qx = SIDE / 4, qy = SIDE / 4;
        const unsigned char *px = image + ((size_t)qy * SIDE + qx) * 4;
        unsigned lit = 0;
        for (unsigned p = 0; p < (unsigned)SIDE * SIDE; ++p)
            if (image[p * 4 + 1] > 128) ++lit;
        const GLenum error = glGetError();
        const unsigned area = (unsigned)SIDE * SIDE;
        const int ok = px[1] > 128 && lit > area / 12 && lit < area / 3;
        fprintf(stderr, "INFO empty_list_then_draw quad=%u,%u,%u lit=%u/%u "
                "err_after_list=0x%04x err=0x%04x\n", px[0], px[1], px[2],
                lit, area, (unsigned)after_list, (unsigned)error);
        result("empty_list_then_draw", ok);
        glDeleteLists(list, 1);
    }

    /* And the same list called twice, then drawn after: a node with a NULL
     * index resource and a count of zero is replayed each time, so if the
     * replay leaves any state behind it compounds. */
    {
        GLuint list = glGenLists(1);
        glNewList(list, GL_COMPILE);
        degenerate(GL_POLYGON, 2);
        glEndList();

        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        for (int n = 0; n < 4; ++n)
            glCallList(list);
        real_quad(0.0f, 1.0f, 0.0f);
        glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);

        const int qx = SIDE / 4, qy = SIDE / 4;
        const unsigned char *px = image + ((size_t)qy * SIDE + qx) * 4;
        unsigned lit = 0;
        for (unsigned p = 0; p < (unsigned)SIDE * SIDE; ++p)
            if (image[p * 4 + 1] > 128) ++lit;
        const int ok = px[1] > 128 && lit > area_guard(SIDE);
        fprintf(stderr, "INFO empty_list_x4_then_draw quad=%u,%u,%u lit=%u\n",
                px[0], px[1], px[2], lit);
        result("empty_list_x4_then_draw", ok);
        glDeleteLists(list, 1);
    }

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        GLuint list = glGenLists(1);
        glNewList(list, GL_COMPILE);
        if (tests[i].degenerate_first) {
            degenerate(tests[i].mode, tests[i].verts);
            real_quad(0.0f, 1.0f, 0.0f);
        } else {
            real_quad(0.0f, 1.0f, 0.0f);
            degenerate(tests[i].mode, tests[i].verts);
        }
        glEndList();

        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glCallList(list);
        glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);

        /* The centre of where the real quad has to be. */
        const int qx = SIDE / 4, qy = SIDE / 4;
        const unsigned char *px = image + ((size_t)qy * SIDE + qx) * 4;
        /* And a count, so a quad drawn in the wrong place or at the wrong
         * size is not passed by one lucky pixel. The quad covers 0.8 x 0.8 of
         * half the viewport each way -- about a sixth of the target. */
        unsigned lit = 0;
        for (unsigned p = 0; p < (unsigned)SIDE * SIDE; ++p)
            if (image[p * 4 + 1] > 128) ++lit;
        const GLenum error = glGetError();
        const unsigned area = (unsigned)SIDE * SIDE;
        const int ok = error == GL_NO_ERROR && px[1] > 128 && px[0] < 128 &&
                       lit > area / 12 && lit < area / 3;
        fprintf(stderr, "INFO %s quad=%u,%u,%u lit=%u/%u err=0x%04x\n",
                tests[i].name, px[0], px[1], px[2], lit, area,
                (unsigned)error);
        result(tests[i].name, ok);
        glDeleteLists(list, 1);
    }
    free(image);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
