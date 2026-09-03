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
 * This probe is the gate for whatever fixes that. It is written to fail until
 * then, and each case isolates one thing:
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
 *
 * GL_LINE_STIPPLE and GL_LINE_SMOOTH are not covered yet; see the note where
 * their cases will go.
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
/* The lit bounding box, printed alongside every count: "nothing in this
 * column" and "nothing anywhere" are different failures and the difference is
 * most of the diagnosis. */
static void lit_bounds(int *x0, int *y0, int *x1, int *y1) {
    *x0 = *y0 = SIZE;
    *x1 = *y1 = -1;
    for (unsigned y = 0; y < SIZE; ++y)
        for (unsigned x = 0; x < SIZE; ++x)
            if (lit(x, y)) {
                if ((int)x < *x0) *x0 = x;
                if ((int)y < *y0) *y0 = y;
                if ((int)x > *x1) *x1 = x;
                if ((int)y > *y1) *y1 = y;
            }
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
        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        fprintf(stderr, "INFO horizontal width=%d box=%d,%d..%d,%d "
                "err=0x%04x\n", mid, x0, y0, x1, y1,
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
        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        fprintf(stderr, "INFO vertical width=%d box=%d,%d..%d,%d err=0x%04x\n",
                mid, x0, y0, x1, y1, (unsigned)glGetError());
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

    /* GL_LINE_STIPPLE and GL_LINE_SMOOTH are not here yet, and are the next
     * two things this file should grow. Both are refused by the rasterizer
     * gate today, so a program that asks for either draws with no rasterizer
     * state at all -- six of the installed programs for stipple, two for
     * smooth. The quad has what they need: stipple wants the distance along
     * the line in pixels, which is a varying the quad shader can emit, and
     * smooth wants a coverage value across the line, which is the `side`
     * attribute interpolated. What stipple additionally needs is a running
     * distance across a LINE_STRIP's segments, which is a prefix sum over
     * the captured vertices rather than anything the quad shader can see. */

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
