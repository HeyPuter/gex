/* Vertically flipped glBlitFramebuffer.
 *
 * GL expresses a flip by giving the source rectangle in the opposite order --
 * glBlitFramebuffer(0, h, w, 0, 0, 0, w, h, ...) -- and st/mesa passes it
 * down as a *negative* box height, with `y` at the top edge, so the rows the
 * box covers are [y - |height|, y). u_blitter normalises it as
 * `box.y += box.height; box.height *= -1`.
 *
 * This driver's transfer-based blit took the absolute value and left y where
 * it was, which moved the box off the end of the texture: the copy became a
 * WebGPU validation error, which costs the blit but raises nothing in the GL
 * API. Minecraft 26.2 ends every frame with exactly this blit -- its render
 * target flipped onto the window -- so it presented pure black at 30 fps with
 * 148 draws a frame behind it and not one refusal in the log.
 *
 * A flip is not an exotic case: it is how any renderer that draws into its own
 * framebuffer gets the image to the screen the right way up, and how
 * glReadPixels of a y-flipped surface is served.
 *
 * Note what this probe can and cannot see. A blit this driver declines falls
 * back to u_blitter, so a broken transfer path still returns the right pixels
 * here and every case below passes on the driver that made Minecraft black --
 * verified by reverting the fix. What made Minecraft's frame black is that
 * u_blitter *also* got it wrong for that one destination (the window's BGRA8
 * texture), silently, with only "blit sent to u_blitter" in the log to say
 * the fast path had been given up on. So these cases are a regression guard
 * over all three flip implementations -- the texture-to-texture copy for
 * equal formats, the transfer for unequal ones, and the blitter behind both --
 * and not a test that any particular one of them ran.
 *
 * The pattern is asymmetric in both axes so that a flip that does nothing, a
 * flip that also mirrors x, and a flip off by a row are all distinguishable.
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

#define W 8
#define H 8

/* Row r, column c -> a byte that identifies both. */
static unsigned char expect_red(unsigned c) { return (unsigned char)(16 + c * 8); }
static unsigned char expect_green(unsigned r) { return (unsigned char)(16 + r * 24); }

static GLuint make_target(GLuint *fbo, GLenum internal_format)
{
    GLuint texture = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexImage2D(GL_TEXTURE_2D, 0, internal_format, W, H, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, *fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           texture, 0);
    return texture;
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
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, W, EGL_HEIGHT, H, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    /* The source, filled by glTexSubImage2D so no shader is involved: the
     * blit is the only thing under test. */
    unsigned char pattern[H][W][4];
    for (unsigned r = 0; r < H; ++r)
        for (unsigned c = 0; c < W; ++c) {
            pattern[r][c][0] = expect_red(c);
            pattern[r][c][1] = expect_green(r);
            pattern[r][c][2] = 32;
            pattern[r][c][3] = 255;
        }

    GLuint src_fbo = 0, dst_fbo = 0, wide_fbo = 0;
    make_target(&src_fbo, GL_RGBA8);
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE,
                    pattern);
    make_target(&dst_fbo, GL_RGBA8);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    /* A destination of a *different* format, which is what Minecraft's blit
     * to the window is: RGBA8 onto the BGRA8 the X visual gives it. Equal
     * formats are a texture-to-texture copy, and this driver has always
     * flipped that correctly; an unequal pair is a pixel transfer, and that
     * is the path the flip was broken in. Without a case like this the probe
     * passes on the broken driver. */
    make_target(&wide_fbo, GL_RGBA16F);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;

    struct {
        const char *name;
        int src_y0, src_y1;         /* as passed to glBlitFramebuffer */
        int flipped;                /* is row r of the destination row H-1-r? */
        int y, height;              /* sub-rectangle cases use these */
        int wide;                   /* blit into the RGBA16F target instead */
    } tests[] = {
        {"upright",            0, H, 0, 0, H, 0},
        {"flipped",            H, 0, 1, 0, H, 0},
        /* A flipped *sub*-rectangle: rows 2..6 of the source, upside down,
         * into rows 2..6 of the destination. An implementation that moves y
         * by the wrong amount lands two rows out and still fills the box. */
        {"flipped-sub",        6, 2, 1, 2, 4, 0},
        {"upright-sub",        2, 6, 0, 2, 4, 0},
        {"upright-convert",    0, H, 0, 0, H, 1},
        {"flipped-convert",    H, 0, 1, 0, H, 1},
        {"flipped-sub-convert", 6, 2, 1, 2, 4, 1},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        const int y = tests[t].y, height = tests[t].height;
        const GLuint target = tests[t].wide ? wide_fbo : dst_fbo;
        glBindFramebuffer(GL_FRAMEBUFFER, target);
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, src_fbo);
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, target);
        glBlitFramebuffer(0, tests[t].src_y0, W, tests[t].src_y1,
                          0, y, W, y + height,
                          GL_COLOR_BUFFER_BIT, GL_NEAREST);
        GLenum error = glGetError();

        unsigned char got[H][W][4];
        memset(got, 0, sizeof(got));
        glBindFramebuffer(GL_READ_FRAMEBUFFER, target);
        glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, got);
        if (error == GL_NO_ERROR) error = glGetError();

        int bad = 0, first_bad_row = -1;
        for (int r = y; r < y + height; ++r) {
            /* Which source row this destination row should hold. The blit
             * covers [y, y+height) of both; a flip reverses within it. */
            const int from = tests[t].flipped ? (y + height - 1 - (r - y)) : r;
            for (unsigned c = 0; c < W; ++c) {
                if (got[r][c][0] != expect_red(c) ||
                    got[r][c][1] != expect_green((unsigned)from)) {
                    if (!bad) first_bad_row = r;
                    ++bad;
                }
            }
        }
        /* Outside the destination rectangle the clear must survive. */
        for (int r = 0; r < H; ++r) {
            if (r >= y && r < y + height) continue;
            for (unsigned c = 0; c < W; ++c)
                if (got[r][c][0] != 0 || got[r][c][1] != 0) {
                    if (!bad) first_bad_row = r;
                    ++bad;
                }
        }
        fprintf(stderr,
                "INFO %s bad=%d first_bad_row=%d row%d=[%u,%u] err=0x%04x\n",
                tests[t].name, bad, first_bad_row, y, got[y][0][0],
                got[y][0][1], (unsigned)error);
        result(tests[t].name, error == GL_NO_ERROR && bad == 0);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
