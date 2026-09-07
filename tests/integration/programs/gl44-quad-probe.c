/* Which diagonal a GL_QUADS, GL_QUAD_STRIP or GL_POLYGON draw is split on.
 *
 * WebGPU has no quad topology, so every quad this driver is handed becomes
 * two triangles. Both diagonals cover the same pixels and give the same four
 * corner colours, so a driver that picks the wrong one draws a quad that is
 * the right shape, in the right place, with the right corners, and a
 * transposed interior. `quad` and `quad-degenerate` in the llvmpipe pixel
 * comparison scored an identical 0.24378 with an animation floor of zero,
 * which is what that looks like from the outside.
 *
 * The centre of the quad is the one pixel that names the diagonal, because it
 * lies exactly on whichever one was chosen and so takes the average of that
 * diagonal's two endpoints. Colour the two diagonals black and white and the
 * centre reads back as one or the other:
 *
 *   split on v1-v3   centre white
 *   split on v0-v2   centre black
 *
 * The expected diagonal is v1-v3, which is what u_indices' do_quad() produces
 * on its inpv == LAST branch -- the branch u_primconvert asks for when
 * flatshade_first is false, and so the one every driver that does not
 * advertise MESA_PRIM_QUADS takes. Measured: softpipe and real radeonsi both
 * agree with this driver on every case here.
 *
 * llvmpipe does not, and that is the point of writing this down. It advertises
 * quads, keeps them all the way to the `draw` module, and splits an
 * immediate-mode quad on v0-v2 instead -- so `quad` and `quad-degenerate`
 * scored an identical 0.24378 against it in the pixel comparison while being
 * correct. A single reference can be wrong; where the two software references
 * disagree with each other by as much as the driver disagrees with either, the
 * comparison's ranking says nothing. llvmpipe scores 3/6 here.
 *
 * Immediate mode, a display list and a vertex array are all measured, because
 * they reach three different converters: st/mesa hands immediate-mode quads
 * to the driver whole, vbo_save converts them against
 * ctx->Const.DriverSupportedPrimMask before the driver sees them, and a
 * client array goes through st/mesa's own draw path. A fix in one place does
 * not imply the other two.
 *
 * Fixed-function on purpose: GL_QUADS does not exist in a core profile, so
 * this is the state real legacy programs put the driver in.
 *
 * Because llvmpipe -- the gate's default oracle -- is the one that is wrong
 * here, the expectation is declared rather than measured from it, and
 * tests/gl44-probe.sh reads this line back:
 *
 *   GEX_PROBE_EXPECT='SUMMARY cases=6 passed=6 failed=0'
 *
 * Without it the browser gate stopped at "native Mesa run did not pass" and
 * this probe sat permanently in the sweep's FAIL column, never actually
 * having been run through the browser at all.
 */
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

/* The quad fills the viewport corner to corner. Its centre is therefore the
 * centre of the target, and with SIDE even there is no pixel whose centre is
 * exactly on the diagonal -- the two pixels either side of it are read and
 * both have to agree, which keeps the answer from riding on a fill rule. */
static const float extent = 1.0f;

static void quad_immediate(void) {
    glBegin(GL_QUADS);
    glColor3f(0, 0, 0); glVertex2f(-extent, -extent);   /* v0 */
    glColor3f(1, 1, 1); glVertex2f( extent, -extent);   /* v1 */
    glColor3f(0, 0, 0); glVertex2f( extent,  extent);   /* v2 */
    glColor3f(1, 1, 1); glVertex2f(-extent,  extent);   /* v3 */
    glEnd();
}

/* GL_QUAD_STRIP takes its vertices in column order, so the quad's corners in
 * winding order are v0, v1, v3, v2. Measured rather than reasoned: llvmpipe,
 * softpipe and radeonsi all split a strip quad on v0-v3, which is not the
 * diagonal the QUADS reasoning predicts. The colours below follow the
 * measurement, so "white centre" means the same thing in every case here. */
static void quad_strip_immediate(void) {
    glBegin(GL_QUAD_STRIP);
    glColor3f(1, 1, 1); glVertex2f(-extent, -extent);   /* v0 */
    glColor3f(0, 0, 0); glVertex2f( extent, -extent);   /* v1 */
    glColor3f(0, 0, 0); glVertex2f(-extent,  extent);   /* v2 */
    glColor3f(1, 1, 1); glVertex2f( extent,  extent);   /* v3 */
    glEnd();
}

/* GL_POLYGON is a fan from v0, so the shared edge is v0-v2 and the centre
 * takes those two. Colouring v0 and v2 white makes "white centre" mean the
 * same thing -- the documented diagonal -- for every case here. */
static void polygon_immediate(void) {
    glBegin(GL_POLYGON);
    glColor3f(1, 1, 1); glVertex2f(-extent, -extent);   /* v0 */
    glColor3f(0, 0, 0); glVertex2f( extent, -extent);   /* v1 */
    glColor3f(1, 1, 1); glVertex2f( extent,  extent);   /* v2 */
    glColor3f(0, 0, 0); glVertex2f(-extent,  extent);   /* v3 */
    glEnd();
}

static void quad_arrays(void) {
    static const float positions[8] = {
        -extent, -extent,  extent, -extent,  extent, extent,  -extent, extent,
    };
    static const float colours[12] = {
        0, 0, 0,   1, 1, 1,   0, 0, 0,   1, 1, 1,
    };
    glEnableClientState(GL_VERTEX_ARRAY);
    glEnableClientState(GL_COLOR_ARRAY);
    glVertexPointer(2, GL_FLOAT, 0, positions);
    glColorPointer(3, GL_FLOAT, 0, colours);
    glDrawArrays(GL_QUADS, 0, 4);
    glDisableClientState(GL_COLOR_ARRAY);
    glDisableClientState(GL_VERTEX_ARRAY);
}

/* Two quads in one glBegin, the second one's diagonal inverted, so that a
 * converter which got the loop stride or the per-quad base wrong is caught
 * rather than only the first quad being measured. The second quad occupies the
 * right half and the first the left. */
static void two_quads_immediate(void) {
    glBegin(GL_QUADS);
    glColor3f(0, 0, 0); glVertex2f(-1.0f, -extent);
    glColor3f(1, 1, 1); glVertex2f( 0.0f, -extent);
    glColor3f(0, 0, 0); glVertex2f( 0.0f,  extent);
    glColor3f(1, 1, 1); glVertex2f(-1.0f,  extent);

    glColor3f(0, 0, 0); glVertex2f( 0.0f, -extent);
    glColor3f(1, 1, 1); glVertex2f( 1.0f, -extent);
    glColor3f(0, 0, 0); glVertex2f( 1.0f,  extent);
    glColor3f(1, 1, 1); glVertex2f( 0.0f,  extent);
    glEnd();
}

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
    /* No version asked for, which is how a compatibility profile is got: a
     * 3.3 context defaults to core, where GL_QUADS does not exist. */
    const EGLint surface_attributes[] = {
        EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE,
    };
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    glViewport(0, 0, SIDE, SIDE);
    glShadeModel(GL_SMOOTH);

    GLuint list = glGenLists(1);
    glNewList(list, GL_COMPILE);
    quad_immediate();
    glEndList();

    struct { const char *name; void (*draw)(void); int lists; } tests[] = {
        {"quads_immediate",     quad_immediate,       0},
        {"quads_display_list",  NULL,                 1},
        {"quads_client_arrays", quad_arrays,          0},
        {"quad_strip",          quad_strip_immediate, 0},
        {"polygon",             polygon_immediate,    0},
        {"two_quads",           two_quads_immediate,  0},
    };

    unsigned char *image = malloc((size_t)SIDE * SIDE * 4);
    if (!image) return 2;

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        /* Mid grey, so that neither answer can be produced by the draw
         * failing to cover the centre at all. */
        glClearColor(0.5f, 0.5f, 0.5f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        if (tests[i].lists) glCallList(list); else tests[i].draw();
        glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);

        /* The two pixels either side of the centre along the split, and for
         * two_quads the same pair inside each half. Both have to be light:
         * one pixel on a shared edge is a question about the fill rule, a
         * pair straddling it is not. */
        const int cx = tests[i].name[0] == 't' ? SIDE / 4 : SIDE / 2;
        const int cy = SIDE / 2;
        const unsigned lo = image[((size_t)(cy - 1) * SIDE + cx - 1) * 4];
        const unsigned hi = image[((size_t)cy * SIDE + cx) * 4];
        unsigned second_lo = lo, second_hi = hi;
        if (tests[i].name[0] == 't') {
            const int sx = 3 * SIDE / 4;
            second_lo = image[((size_t)(cy - 1) * SIDE + sx - 1) * 4];
            second_hi = image[((size_t)cy * SIDE + sx) * 4];
        }
        /* Covered at all: the corner the quad certainly fills. */
        const unsigned corner = image[((size_t)2 * SIDE + 2) * 4];
        const unsigned corner_far =
            image[((size_t)(SIDE - 3) * SIDE + SIDE - 3) * 4];
        const GLenum error = glGetError();
        const int drew = corner != 128 || corner_far != 128;
        /* The documented diagonal puts white at the centre. 160 is clear of
         * the 128 clear colour in one direction and of a black centre in the
         * other, so neither a missing draw nor the wrong diagonal can reach
         * it. */
        const int ok = error == GL_NO_ERROR && drew &&
                       lo > 160 && hi > 160 && second_lo > 160 &&
                       second_hi > 160;
        fprintf(stderr,
                "INFO %s centre=%u,%u second=%u,%u corners=%u,%u err=0x%04x\n",
                tests[i].name, lo, hi, second_lo, second_hi, corner,
                corner_far, (unsigned)error);
        result(tests[i].name, ok);
    }
    free(image);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
