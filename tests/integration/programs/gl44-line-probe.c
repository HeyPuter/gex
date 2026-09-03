/* Wide, smooth and stippled lines, none of which WebGPU has.
 *
 * A WebGPU line is one pixel, there is no stipple and there is no line
 * antialiasing -- the same three holes points had before the quad expansion.
 * `caps->max_line_width` is 1.0, so st/mesa clamps glLineWidth before the
 * driver ever sees it, exactly as it clamped point size; ten of the installed
 * programs are refused outright for line stipple or line smoothing, and an
 * unknown number are silently drawing one-pixel lines where they asked for
 * more.
 *
 * This probe is the gate for the quad expansion that fixes it, and it was
 * written *before* the feature and against llvmpipe -- which is how the rule
 * got settled before the shader did. Each case isolates one thing:
 *
 *  - the *width*, measured across a horizontal and a vertical line, because
 *    an expansion that works in the wrong space gets one axis right and the
 *    other wrong;
 *  - a diagonal, which is where the *rule* shows: GL offsets w copies along
 *    the line's minor axis, so a five-pixel diagonal lights five pixels per
 *    row and a rotated perpendicular quad would light about seven;
 *  - the varyings, which have to interpolate along the line: a quad built
 *    from two endpoints carries one endpoint's values on each end, and an
 *    implementation that carried the *first* endpoint's on all four corners
 *    draws a flat line that looks correct until you check the colours;
 *  - GL_LINE_STIPPLE, twice: once on an independent segment, where GL
 *    restarts the counter, and once across a LINE_STRIP, where it runs on --
 *    the second compared against the first pixel for pixel, because a
 *    counter that restarted at the join could still give the same count;
 *  - GL_LINE_SMOOTH, which is a *different rule*: an antialiased wide line is
 *    a rectangle of width w centred on the segment, so a smooth diagonal
 *    reaches wider than an aliased one of the same width, and the fragment's
 *    coverage of that rectangle multiplies into alpha.
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

#define SIZE 32
static unsigned char pixels[SIZE * SIZE * 4];

static void read_target(void) {
    memset(pixels, 0xAB, sizeof(pixels));
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
}
static unsigned char channel(unsigned x, unsigned y, unsigned c) {
    return pixels[(y * SIZE + x) * 4 + c];
}
static int lit(unsigned x, unsigned y) {
    return channel(x, y, 0) > 40 || channel(x, y, 1) > 40 ||
           channel(x, y, 2) > 40;
}
static void clear_black(void) {
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
}
/* How many pixels are lit in one column, and in one row. */
static int lit_in_column(unsigned x) {
    int n = 0;
    for (unsigned y = 0; y < SIZE; ++y) n += lit(x, y);
    return n;
}
static int lit_in_row(unsigned y) {
    int n = 0;
    for (unsigned x = 0; x < SIZE; ++x) n += lit(x, y);
    return n;
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
    const EGLint surface_attributes[] = {EGL_WIDTH, SIZE, EGL_HEIGHT, SIZE,
                                         EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    {
        GLfloat range[2] = {0.0f, 0.0f};
        glGetFloatv(GL_ALIASED_LINE_WIDTH_RANGE, range);
        fprintf(stderr, "INFO line width range %.1f..%.1f\n", range[0],
                range[1]);
    }

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIZE, SIZE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIZE, SIZE);
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();

    /* 1. A horizontal line five pixels wide: every column it crosses should
     * have five lit pixels. Sampled away from the ends, where the end rule
     * has its own say. */
    {
        clear_black();
        glColor3f(1.0f, 1.0f, 1.0f);
        glLineWidth(5.0f);
        glBegin(GL_LINES);
        glVertex2f(-0.8f, 0.0f);
        glVertex2f(0.8f, 0.0f);
        glEnd();
        read_target();
        const int mid = lit_in_column(SIZE / 2);
        fprintf(stderr, "INFO horizontal width=%d err=0x%04x\n", mid,
                (unsigned)glGetError());
        result("horizontal_width", mid >= 4 && mid <= 6);
    }

    /* 2. The same vertically, which is the axis a wrong perpendicular gets
     * wrong on its own. */
    {
        clear_black();
        glLineWidth(5.0f);
        glBegin(GL_LINES);
        glVertex2f(0.0f, -0.8f);
        glVertex2f(0.0f, 0.8f);
        glEnd();
        read_target();
        const int mid = lit_in_row(SIZE / 2);
        fprintf(stderr, "INFO vertical width=%d err=0x%04x\n", mid,
                (unsigned)glGetError());
        result("vertical_width", mid >= 4 && mid <= 6);
    }

    /* 3. A diagonal, which is where the *rule* shows. GL rasterises a wide
     * line as w copies offset along its minor axis, so a five-pixel diagonal
     * still lights five pixels per row; a rotated perpendicular quad would
     * light about seven. llvmpipe says five. */
    {
        clear_black();
        glLineWidth(5.0f);
        glBegin(GL_LINES);
        glVertex2f(-0.7f, -0.7f);
        glVertex2f(0.7f, 0.7f);
        glEnd();
        read_target();
        const int mid = lit_in_row(SIZE / 2);
        fprintf(stderr, "INFO diagonal row=%d err=0x%04x\n", mid,
                (unsigned)glGetError());
        /* Five, not seven: GL offsets the copies along the minor axis, so a
         * diagonal is as many pixels per row as it is wide. A rotated
         * perpendicular quad would give about sqrt(2) times more, and would
         * be the wrong rule rather than a rounding difference. */
        result("diagonal_width", mid >= 4 && mid <= 6);
    }

    /* 4. Varyings along the line. Red at one end, blue at the other, and the
     * two ends have to differ -- a quad carrying one endpoint's colour on all
     * four corners draws a line that is the right shape and one flat colour.
     */
    {
        clear_black();
        glLineWidth(5.0f);
        glShadeModel(GL_SMOOTH);
        glBegin(GL_LINES);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f(-0.8f, 0.0f);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f(0.8f, 0.0f);
        glEnd();
        read_target();
        /* The row the line actually landed on, not the one arithmetic says
         * it should: a one-pixel line at an exact coordinate is a tie, and
         * which side it falls on is not what this case is about. */
        unsigned y = SIZE / 2;
        int best = -1;
        for (unsigned row = 0; row < SIZE; ++row) {
            const int n = lit_in_row(row);
            if (n > best) { best = n; y = row; }
        }
        const unsigned left = 5, right = SIZE - 6;
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(left, y, 0) > channel(right, y, 0) + 60 &&
                       channel(right, y, 2) > channel(left, y, 2) + 60;
        fprintf(stderr, "INFO gradient left=%u,%u,%u right=%u,%u,%u\n",
                channel(left, y, 0), channel(left, y, 1), channel(left, y, 2),
                channel(right, y, 0), channel(right, y, 1),
                channel(right, y, 2));
        result("varyings_interpolate", ok);
    }

    /* 5. GL_LINE_STIPPLE. A 1-pixel line with the pattern 0x00FF and factor 1
     * is eight pixels on, eight off, so a 24-pixel span has two thirds of its
     * pixels lit rather than all of them. */
    {
        clear_black();
        glLineWidth(1.0f);
        glLineStipple(1, 0x00FF);
        glEnable(GL_LINE_STIPPLE);
        glBegin(GL_LINES);
        glVertex2f(-0.75f, 0.0f);
        glVertex2f(0.75f, 0.0f);
        glEnd();
        glDisable(GL_LINE_STIPPLE);
        read_target();
        int span = 0, on = 0;
        for (unsigned x = 0; x < SIZE; ++x) {
            const int any = lit(x, SIZE / 2) || lit(x, SIZE / 2 - 1);
            if (x >= 5 && x < SIZE - 5) { ++span; on += any; }
        }
        fprintf(stderr, "INFO stipple on=%d of %d err=0x%04x\n", on, span,
                (unsigned)glGetError());
        /* Not every pixel, and not none: the pattern has to be visible. */
        result("line_stipple", on > 2 && on < span - 2);
    }

    /* 5b. The same pattern across a LINE_STRIP, which is where the counter
     * has to run *on*: GL restarts it at every independent segment and never
     * between the segments of a strip. Drawn as two collinear halves and
     * compared against the single line above, pixel for pixel -- a counter
     * that restarted at the join would give a different image, and might
     * still give the same *count*, which is why this compares the picture. */
    {
        unsigned char single[SIZE * SIZE * 4];
        glLineStipple(1, 0x00FF);
        glEnable(GL_LINE_STIPPLE);
        clear_black();
        glBegin(GL_LINES);
        glVertex2f(-0.75f, 0.0f);
        glVertex2f(0.75f, 0.0f);
        glEnd();
        read_target();
        memcpy(single, pixels, sizeof(single));

        clear_black();
        glBegin(GL_LINE_STRIP);
        glVertex2f(-0.75f, 0.0f);
        glVertex2f(0.0f, 0.0f);
        glVertex2f(0.75f, 0.0f);
        glEnd();
        glDisable(GL_LINE_STIPPLE);
        read_target();
        const GLenum error = glGetError();
        const int same = memcmp(single, pixels, sizeof(single)) == 0;
        int lit_count = 0;
        for (unsigned y = 0; y < SIZE; ++y) lit_count += lit_in_row(y);
        fprintf(stderr, "INFO stipple_strip lit=%d same=%d err=0x%04x\n",
                lit_count, same, (unsigned)error);
        result("line_stipple_strip",
               error == GL_NO_ERROR && lit_count > 0 && same);
    }

    /* 6. GL_LINE_SMOOTH on a wide line: the corners of the end caps come off,
     * so the count of lit pixels drops from the aliased case. */
    {
        clear_black();
        glLineWidth(5.0f);
        glBegin(GL_LINES);
        glVertex2f(-0.5f, -0.5f);
        glVertex2f(0.5f, 0.5f);
        glEnd();
        read_target();
        int aliased = 0;
        for (unsigned y = 0; y < SIZE; ++y) aliased += lit_in_row(y);

        clear_black();
        glEnable(GL_LINE_SMOOTH);
        glBegin(GL_LINES);
        glVertex2f(-0.5f, -0.5f);
        glVertex2f(0.5f, 0.5f);
        glEnd();
        glDisable(GL_LINE_SMOOTH);
        read_target();
        int smooth = 0;
        for (unsigned y = 0; y < SIZE; ++y) smooth += lit_in_row(y);
        fprintf(stderr, "INFO smooth aliased=%d smooth=%d err=0x%04x\n",
                aliased, smooth, (unsigned)glGetError());
        result("line_smooth_differs", aliased > 0 && smooth > 0 &&
                                      aliased != smooth);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
