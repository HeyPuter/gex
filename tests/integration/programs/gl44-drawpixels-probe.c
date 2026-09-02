/* The pixel-rectangle family, and glCallLists.
 *
 * glDrawPixels was missing from the guest's libGL, which meant ten of the
 * installed programs failed to resolve the symbol and never started. It
 * carries an image, so it is the one of this batch where the wire shape can be
 * wrong in a way that still draws something -- hence a probe that checks
 * *which* pixels landed where, on a target big enough to tell a row-order or
 * stride mistake from a correct copy.
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

static unsigned char pixels[8 * 8 * 4];

static void read_target(void) {
    memset(pixels, 0xAB, sizeof(pixels));
    glReadPixels(0, 0, 8, 8, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
}
static unsigned char channel(unsigned x, unsigned y, unsigned c) {
    return pixels[(y * 8 + x) * 4 + c];
}
static void clear_black(void) {
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 8, 8, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 8, 8);
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    glOrtho(0.0, 8.0, 0.0, 8.0, -1.0, 1.0);
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glPixelStorei(GL_PACK_ALIGNMENT, 1);

    /* A 2x2 image with four different colours, so the row order and the
     * within-row order are both observable. GL's first row is the bottom. */
    static const unsigned char image[16] = {
        255, 0, 0, 255,      0, 255, 0, 255,     /* bottom row */
        0, 0, 255, 255,      255, 255, 0, 255,   /* top row */
    };

    /* 1. glDrawPixels at a raster position away from the origin, so a
     * misplaced rectangle is not hidden by landing at (0,0) anyway. */
    clear_black();
    glRasterPos2i(2, 3);
    glDrawPixels(2, 2, GL_RGBA, GL_UNSIGNED_BYTE, image);
    read_target();
    {
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(2, 3, 0) > 200 && channel(2, 3, 1) < 60 &&
                       channel(3, 3, 1) > 200 && channel(3, 3, 0) < 60 &&
                       channel(2, 4, 2) > 200 &&
                       channel(3, 4, 0) > 200 && channel(3, 4, 1) > 200 &&
                       /* and nothing outside it */
                       channel(0, 0, 0) < 40 && channel(7, 7, 0) < 40;
        fprintf(stderr, "INFO drawpixels (2,3)=%u,%u,%u (3,3)=%u,%u,%u "
                "(2,4)=%u,%u,%u (3,4)=%u,%u,%u\n",
                channel(2, 3, 0), channel(2, 3, 1), channel(2, 3, 2),
                channel(3, 3, 0), channel(3, 3, 1), channel(3, 3, 2),
                channel(2, 4, 0), channel(2, 4, 1), channel(2, 4, 2),
                channel(3, 4, 0), channel(3, 4, 1), channel(3, 4, 2));
        result("draw_pixels", ok);
    }

    /* 2. glCopyPixels moving that rectangle somewhere else in the same
     * framebuffer, which reads the target as well as writing it. */
    glRasterPos2i(5, 0);
    glCopyPixels(2, 3, 2, 2, GL_COLOR);
    read_target();
    {
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(5, 0, 0) > 200 && channel(5, 0, 1) < 60 &&
                       channel(6, 0, 1) > 200 &&
                       channel(5, 1, 2) > 200;
        fprintf(stderr, "INFO copypixels (5,0)=%u,%u,%u (6,0)=%u,%u,%u "
                "(5,1)=%u,%u,%u\n",
                channel(5, 0, 0), channel(5, 0, 1), channel(5, 0, 2),
                channel(6, 0, 0), channel(6, 0, 1), channel(6, 0, 2),
                channel(5, 1, 0), channel(5, 1, 1), channel(5, 1, 2));
        result("copy_pixels", ok);
    }

    /* 3. glListBase and glCallLists: two lists, each drawing one rectangle,
     * called by offset so the base has to be applied. */
    {
        const GLuint base = glGenLists(2);
        if (!base) {
            result("call_lists", 0);
        } else {
            glNewList(base + 0, GL_COMPILE);
            glColor3f(0.0f, 1.0f, 1.0f);
            glRectf(0.0f, 6.0f, 2.0f, 8.0f);
            glEndList();
            glNewList(base + 1, GL_COMPILE);
            glColor3f(1.0f, 0.0f, 1.0f);
            glRectf(6.0f, 6.0f, 8.0f, 8.0f);
            glEndList();
            clear_black();
            glListBase(base);
            static const unsigned char which[2] = {0, 1};
            glCallLists(2, GL_UNSIGNED_BYTE, which);
            read_target();
            const int ok = glGetError() == GL_NO_ERROR &&
                           channel(1, 7, 1) > 200 && channel(1, 7, 2) > 200 &&
                           channel(1, 7, 0) < 60 &&
                           channel(6, 7, 0) > 200 && channel(6, 7, 2) > 200 &&
                           channel(6, 7, 1) < 60;
            fprintf(stderr, "INFO calllists left=%u,%u,%u right=%u,%u,%u\n",
                    channel(1, 7, 0), channel(1, 7, 1), channel(1, 7, 2),
                    channel(6, 7, 0), channel(6, 7, 1), channel(6, 7, 2));
            result("call_lists", ok);
            glListBase(0);
        }
    }

    /* 4. glBitmap, whose one-bit-per-pixel packing is the only image on this
     * wire that the shared pixel-store calculation refuses -- its rows are
     * measured separately at both ends, so a mismatch is a wrong row stride
     * rather than a wrong colour. Two rows of 8 bits: the bottom row is the
     * left half set, the top row the right half. */
    {
        static const unsigned char bits[2] = {0xF0, 0x0F};
        clear_black();
        glColor3f(1.0f, 1.0f, 1.0f);
        glRasterPos2i(0, 0);
        glBitmap(8, 2, 0.0f, 0.0f, 0.0f, 0.0f, bits);
        read_target();
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(0, 0, 0) > 200 && channel(7, 0, 0) < 60 &&
                       channel(0, 1, 0) < 60 && channel(7, 1, 0) > 200;
        fprintf(stderr, "INFO bitmap row0=%u,%u row1=%u,%u\n",
                channel(0, 0, 0), channel(7, 0, 0),
                channel(0, 1, 0), channel(7, 1, 0));
        result("bitmap", ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
