/* Rendering into a mipmap level other than the base, and what the rest of the
 * chain holds afterwards.
 *
 * `tri-fbo-tex-mip` is the top deterministic divergence in the llvmpipe pixel
 * comparison that both software references agree on: llvmpipe and softpipe
 * produce byte-identical images and this driver produces a different one, so
 * unlike `quad` it is not a reference quirk. The demo attaches one 32x32 level
 * of a 512x512 ten-level texture to a framebuffer, draws into it, and then
 * samples the whole chain through a perspective matrix -- and the difference
 * is entirely in the levels it did *not* draw into, which come out black here
 * and hold a gradient everywhere else.
 *
 * So the questions are, in order:
 *
 *   level_write      does a draw into level n land in level n at all
 *   level_isolation  does it leave the other levels alone
 *   generate_mipmap  does glGenerateMipmap fill the chain from the base
 *   generate_from_n  does it fill from a *non-zero* base level, which is the
 *                    case that needs GL_TEXTURE_BASE_LEVEL to be honoured
 *   sample_level     does sampling with an explicit lod read the level that
 *                    was written
 *
 * Every level is read back with glGetTexImage rather than sampled, so a
 * failure names the level that is wrong instead of the pixel that is.
 */
#include <EGL/egl.h>
#define GL_GLEXT_PROTOTYPES 1
#include <GL/gl.h>
#include <GL/glext.h>
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

#define BASE 64          /* 64 32 16 8 4 2 1 -- seven levels */
#define LEVELS 7

typedef void (*gen_mipmap_fn)(GLenum);
static gen_mipmap_fn generate_mipmap;

/* A distinct, recognisable colour per level, so that a level holding another
 * level's content is named rather than merely reported as wrong. */
static void level_colour(int level, float *rgba) {
    rgba[0] = (float)(level + 1) / 8.0f;
    rgba[1] = (float)(LEVELS - level) / 8.0f;
    rgba[2] = level & 1 ? 1.0f : 0.25f;
    rgba[3] = 1.0f;
}

static unsigned expect_byte(float v) {
    int b = (int)(v * 255.0f + 0.5f);
    return b < 0 ? 0 : b > 255 ? 255 : (unsigned)b;
}

/* The centre texel of a level, which is inside the drawn quad on every level
 * including the 1x1 one. */
static void read_level(GLuint texture, int level, unsigned char *out) {
    const int side = BASE >> level;
    unsigned char *image = malloc((size_t)side * side * 4);
    memset(image, 0xAB, (size_t)side * side * 4);
    glBindTexture(GL_TEXTURE_2D, texture);
    glGetTexImage(GL_TEXTURE_2D, level, GL_RGBA, GL_UNSIGNED_BYTE, image);
    const size_t centre = ((size_t)(side / 2) * side + side / 2) * 4;
    memcpy(out, image + centre, 4);
    free(image);
}

/* A full-target quad in the given colour, drawn rather than cleared: a clear
 * can be serviced by a path that never touches the attachment's level, and
 * this probe is about where a *draw* lands. */
static void fill(float r, float g, float b) {
    glColor4f(r, g, b, 1.0f);
    glBegin(GL_TRIANGLE_STRIP);
    glVertex2f(-1.0f, -1.0f);
    glVertex2f( 1.0f, -1.0f);
    glVertex2f(-1.0f,  1.0f);
    glVertex2f( 1.0f,  1.0f);
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    generate_mipmap = (gen_mipmap_fn)eglGetProcAddress("glGenerateMipmap");
    if (!generate_mipmap)
        generate_mipmap = (gen_mipmap_fn)eglGetProcAddress("glGenerateMipmapEXT");

    GLuint texture = 0, framebuffer = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    for (int level = 0; level < LEVELS; ++level)
        glTexImage2D(GL_TEXTURE_2D, level, GL_RGBA8, BASE >> level,
                     BASE >> level, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, LEVELS - 1);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST_MIPMAP_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &framebuffer);

    /* Every level drawn into, each its own colour. */
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    for (int level = 0; level < LEVELS; ++level) {
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, texture, level);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
            fprintf(stderr, "INFO incomplete at level %d\n", level);
            printf("SUMMARY cases=1 passed=0 failed=1\n");
            return 1;
        }
        glViewport(0, 0, BASE >> level, BASE >> level);
        float want[4];
        level_colour(level, want);
        fill(want[0], want[1], want[2]);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, 0);

    int all_levels_ok = 1;
    for (int level = 0; level < LEVELS; ++level) {
        unsigned char got[4];
        read_level(texture, level, got);
        float want[4];
        level_colour(level, want);
        const int ok = abs((int)got[0] - (int)expect_byte(want[0])) <= 2 &&
                       abs((int)got[1] - (int)expect_byte(want[1])) <= 2 &&
                       abs((int)got[2] - (int)expect_byte(want[2])) <= 2;
        fprintf(stderr, "INFO level_write l=%d got=%u,%u,%u want=%u,%u,%u\n",
                level, got[0], got[1], got[2], expect_byte(want[0]),
                expect_byte(want[1]), expect_byte(want[2]));
        if (!ok) all_levels_ok = 0;
    }
    result("level_write_all", all_levels_ok);

    /* One level redrawn, and every other level has to be untouched. This is
     * the shape of the tri-fbo-tex-mip difference: the levels that were not
     * drawn into. */
    {
        const int target = 2;
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, texture, target);
        glViewport(0, 0, BASE >> target, BASE >> target);
        fill(1.0f, 0.0f, 1.0f);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);

        int isolated = 1;
        for (int level = 0; level < LEVELS; ++level) {
            unsigned char got[4];
            read_level(texture, level, got);
            float want[4];
            level_colour(level, want);
            unsigned wr = expect_byte(want[0]), wg = expect_byte(want[1]),
                     wb = expect_byte(want[2]);
            if (level == target) { wr = 255; wg = 0; wb = 255; }
            const int ok = abs((int)got[0] - (int)wr) <= 2 &&
                           abs((int)got[1] - (int)wg) <= 2 &&
                           abs((int)got[2] - (int)wb) <= 2;
            if (!ok) {
                isolated = 0;
                fprintf(stderr,
                        "INFO level_isolation l=%d got=%u,%u,%u want=%u,%u,%u\n",
                        level, got[0], got[1], got[2], wr, wg, wb);
            }
        }
        fprintf(stderr, "INFO level_isolation target=%d clean=%d\n", target,
                isolated);
        result("level_isolation", isolated);
    }

    /* glGenerateMipmap from the base level: every level becomes the base
     * colour, because the base is a flat fill. */
    if (generate_mipmap) {
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, texture, 0);
        glViewport(0, 0, BASE, BASE);
        fill(0.25f, 0.5f, 0.75f);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        glBindTexture(GL_TEXTURE_2D, texture);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_BASE_LEVEL, 0);
        generate_mipmap(GL_TEXTURE_2D);
        int chain_ok = 1;
        for (int level = 0; level < LEVELS; ++level) {
            unsigned char got[4];
            read_level(texture, level, got);
            const int ok = abs((int)got[0] - 64) <= 3 &&
                           abs((int)got[1] - 128) <= 3 &&
                           abs((int)got[2] - 191) <= 3;
            if (!ok) {
                chain_ok = 0;
                fprintf(stderr, "INFO generate_mipmap l=%d got=%u,%u,%u "
                        "want=64,128,191\n", level, got[0], got[1], got[2]);
            }
        }
        fprintf(stderr, "INFO generate_mipmap chain_ok=%d\n", chain_ok);
        result("generate_mipmap", chain_ok);

        /* The same from a non-zero base level. Levels below the base keep
         * what they had -- GL fills from BASE_LEVEL down to MAX_LEVEL -- so
         * this separates "generates a chain" from "generates the right one".
         *
         * Three colours are in play on purpose, because the three ways this
         * can fail are otherwise indistinguishable in the readback. Level 0
         * is repainted green *without* regenerating, level 2 orange, and the
         * chain is generated from level 2. Levels 3 and below then read:
         *
         *   orange (255,128,0)   correct
         *   green  (0,255,0)     generated from level 0, ignoring BASE_LEVEL
         *   blue   (64,128,191)  untouched -- the generate did nothing
         *
         * The first version of this probe left level 0 holding the same
         * colour as the rest of the chain, and "did nothing" and "read the
         * wrong level" produced identical numbers. */
        const int base = 2;
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, texture, 0);
        glViewport(0, 0, BASE, BASE);
        fill(0.0f, 1.0f, 0.0f);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, texture, base);
        glViewport(0, 0, BASE >> base, BASE >> base);
        fill(1.0f, 0.5f, 0.0f);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);

        /* The chain as it stands *before* generating, because two different
         * faults produce identical numbers afterwards: a generate that reads
         * level 0 instead of the base, and a level-0 fill that leaked into
         * the whole chain and a generate that did nothing. Level 1 is outside
         * the generate's range either way, so it is the one that separates
         * them -- green here means the fill leaked. */
        for (int level = 0; level < LEVELS; ++level) {
            unsigned char got[4];
            read_level(texture, level, got);
            fprintf(stderr, "INFO pre_generate l=%d got=%u,%u,%u\n", level,
                    got[0], got[1], got[2]);
        }

        glBindTexture(GL_TEXTURE_2D, texture);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_BASE_LEVEL, base);
        generate_mipmap(GL_TEXTURE_2D);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_BASE_LEVEL, 0);
        int from_n_ok = 1;
        for (int level = base; level < LEVELS; ++level) {
            unsigned char got[4];
            read_level(texture, level, got);
            const int ok = abs((int)got[0] - 255) <= 3 &&
                           abs((int)got[1] - 128) <= 3 &&
                           abs((int)got[2] - 0) <= 3;
            if (!ok) {
                from_n_ok = 0;
                fprintf(stderr, "INFO generate_from_n l=%d got=%u,%u,%u "
                        "want=255,128,0\n", level, got[0], got[1], got[2]);
            }
        }
        /* Levels 0 and 1 are below the base and must be left as they were --
         * green, not regenerated. */
        unsigned char below[4], at_base[4], mid[4];
        read_level(texture, 0, below);
        read_level(texture, base, at_base);
        read_level(texture, 1, mid);
        fprintf(stderr, "INFO post_generate l1=%u,%u,%u (blue 64,128,191 means "
                "untouched, green 0,255,0 means the level 0 fill leaked)\n",
                mid[0], mid[1], mid[2]);
        const int below_kept = below[0] <= 3 && below[1] >= 252 &&
                               below[2] <= 3;
        /* The base level is the source, so generating must not overwrite it. */
        const int base_kept = abs((int)at_base[0] - 255) <= 3 &&
                              abs((int)at_base[1] - 128) <= 3 &&
                              at_base[2] <= 3;
        fprintf(stderr, "INFO generate_from_n chain_ok=%d below_kept=%d "
                "base_kept=%d l0=%u,%u,%u l%d=%u,%u,%u\n", from_n_ok,
                below_kept, base_kept, below[0], below[1], below[2], base,
                at_base[0], at_base[1], at_base[2]);
        result("generate_from_base_level", from_n_ok && below_kept && base_kept);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
