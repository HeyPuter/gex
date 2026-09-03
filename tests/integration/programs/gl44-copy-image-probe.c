/* ARB_copy_image -- one of GL 4.3's five remaining items.
 *
 * glCopyImageSubData copies pixels between two images with no framebuffer, no
 * shader and no format conversion: it is a memory copy that happens to know
 * about mip levels, array layers and block sizes. Texture streaming systems
 * use it to promote a staged upload into a resident atlas, and every engine
 * that manages its own texture memory reaches for it eventually.
 *
 * What makes it more than a memcpy is the *compatibility class* rule: the two
 * images need not have the same format, only the same bytes per block. So
 * DXT1 (8 bytes per 4x4 block) may be copied to and from RG32UI (8 bytes per
 * texel), and a 16x16 DXT1 image corresponds to a 4x4 RG32UI one. That pairing
 * is what st/mesa's cap is named after --
 * `copy_between_compressed_and_plain_formats` -- and it is what a driver on an
 * API that demands identical formats for a texture-to-texture copy has to
 * work around. WebGPU is exactly such an API, so the interesting cases here
 * are the ones where the formats differ.
 *
 *   `same-format`     RGBA8 to RGBA8, whole image -- the degenerate case
 *   `subregion`       a 4x4 window at an offset in both images, which is
 *                     where a driver that ignores the origins still passes
 *                     the first case
 *   `array-layer`     layer 2 of an array to layer 0 of another
 *   `mip-level`       level 1 to level 1, since the extent is the *level's*
 *   `class-8byte`     RG32UI to RGBA16UI: different formats, same 8 bytes,
 *                     no conversion -- the bits must arrive unchanged, which
 *                     a driver that blits through a shader gets wrong
 *   `compressed`      DXT1 to RG32UI and back, the pairing the cap is named
 *                     for; skipped, not failed, where S3TC is absent
 *
 * Every case compares the bytes exactly. There is no filtering and no
 * conversion in any of them, so a tolerance would only hide a defect.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
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

static int have_extension(const char *want)
{
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 16

/* A recognisable byte at every position, so a copy landing at the wrong
 * offset shows up as the wrong value rather than as plausible noise. */
static void fill(unsigned char *bytes, size_t n, unsigned seed)
{
    for (size_t i = 0; i < n; ++i)
        bytes[i] = (unsigned char)(i * 7u + seed * 31u + 1u);
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have = have_extension("GL_ARB_copy_image");
    const int have_s3tc =
        have_extension("GL_EXT_texture_compression_s3tc") ||
        have_extension("GL_ARB_texture_compression_bptc");
    fprintf(stderr, "INFO version=%s copy_image=%d s3tc=%d\n",
            glGetString(GL_VERSION), have, have_s3tc);

    /* Case 1 and 2: RGBA8 to RGBA8, whole image and a window. */
    for (unsigned pass = 0; pass < 2; ++pass) {
        const char *name = pass == 0 ? "same-format" : "subregion";
        unsigned char src_bytes[SIDE * SIDE * 4];
        unsigned char dst_bytes[SIDE * SIDE * 4];
        unsigned char got[SIDE * SIDE * 4];
        fill(src_bytes, sizeof(src_bytes), 1);
        fill(dst_bytes, sizeof(dst_bytes), 2);
        GLuint tex[2] = {0, 0};
        glGenTextures(2, tex);
        for (unsigned t = 0; t < 2; ++t) {
            glBindTexture(GL_TEXTURE_2D, tex[t]);
            glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, SIDE, SIDE);
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIDE, SIDE, GL_RGBA,
                            GL_UNSIGNED_BYTE, t == 0 ? src_bytes : dst_bytes);
        }
        const unsigned sx = pass == 0 ? 0 : 3, sy = pass == 0 ? 0 : 5;
        const unsigned dx = pass == 0 ? 0 : 9, dy = pass == 0 ? 0 : 2;
        const unsigned w = pass == 0 ? SIDE : 4, h = pass == 0 ? SIDE : 4;
        if (have)
            glCopyImageSubData(tex[0], GL_TEXTURE_2D, 0, sx, sy, 0,
                               tex[1], GL_TEXTURE_2D, 0, dx, dy, 0, w, h, 1);
        glBindTexture(GL_TEXTURE_2D, tex[1]);
        memset(got, 0, sizeof(got));
        glGetTexImage(GL_TEXTURE_2D, 0, GL_RGBA, GL_UNSIGNED_BYTE, got);
        /* Build what the destination should be, on the CPU. */
        unsigned char want[SIDE * SIDE * 4];
        memcpy(want, dst_bytes, sizeof(want));
        for (unsigned y = 0; y < h; ++y)
            memcpy(want + ((dy + y) * SIDE + dx) * 4,
                   src_bytes + ((sy + y) * SIDE + sx) * 4, w * 4);
        const GLenum error = glGetError();
        unsigned differ = 0;
        for (size_t i = 0; i < sizeof(want); ++i)
            if (want[i] != got[i]) ++differ;
        fprintf(stderr, "INFO %s differ=%u err=0x%04x\n", name, differ,
                (unsigned)error);
        result(name, have && error == GL_NO_ERROR && differ == 0);
        glDeleteTextures(2, tex);
    }

    /* Case 3: one array layer to another. */
    {
        const unsigned layers = 4;
        unsigned char src_bytes[SIDE * SIDE * 4 * 4];
        unsigned char got[SIDE * SIDE * 4];
        fill(src_bytes, sizeof(src_bytes), 3);
        GLuint tex[2] = {0, 0};
        glGenTextures(2, tex);
        for (unsigned t = 0; t < 2; ++t) {
            glBindTexture(GL_TEXTURE_2D_ARRAY, tex[t]);
            glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_RGBA8, SIDE, SIDE,
                           layers);
            if (t == 0)
                glTexSubImage3D(GL_TEXTURE_2D_ARRAY, 0, 0, 0, 0, SIDE, SIDE,
                                layers, GL_RGBA, GL_UNSIGNED_BYTE, src_bytes);
        }
        if (have)
            glCopyImageSubData(tex[0], GL_TEXTURE_2D_ARRAY, 0, 0, 0, 2,
                               tex[1], GL_TEXTURE_2D_ARRAY, 0, 0, 0, 0,
                               SIDE, SIDE, 1);
        glBindTexture(GL_TEXTURE_2D_ARRAY, tex[1]);
        unsigned char all[SIDE * SIDE * 4 * 4] = {0};
        glGetTexImage(GL_TEXTURE_2D_ARRAY, 0, GL_RGBA, GL_UNSIGNED_BYTE, all);
        memcpy(got, all, sizeof(got));
        const GLenum error = glGetError();
        unsigned differ = 0;
        for (size_t i = 0; i < sizeof(got); ++i)
            if (got[i] != src_bytes[SIDE * SIDE * 4 * 2 + i]) ++differ;
        fprintf(stderr, "INFO array-layer differ=%u err=0x%04x\n", differ,
                (unsigned)error);
        result("array-layer", have && error == GL_NO_ERROR && differ == 0);
        glDeleteTextures(2, tex);
    }

    /* Case 4: level 1 to level 1, whose extent is half the base's. */
    {
        const unsigned half = SIDE / 2;
        unsigned char src_bytes[(SIDE / 2) * (SIDE / 2) * 4];
        unsigned char got[(SIDE / 2) * (SIDE / 2) * 4];
        fill(src_bytes, sizeof(src_bytes), 4);
        GLuint tex[2] = {0, 0};
        glGenTextures(2, tex);
        for (unsigned t = 0; t < 2; ++t) {
            glBindTexture(GL_TEXTURE_2D, tex[t]);
            glTexStorage2D(GL_TEXTURE_2D, 2, GL_RGBA8, SIDE, SIDE);
            if (t == 0)
                glTexSubImage2D(GL_TEXTURE_2D, 1, 0, 0, half, half, GL_RGBA,
                                GL_UNSIGNED_BYTE, src_bytes);
        }
        if (have)
            glCopyImageSubData(tex[0], GL_TEXTURE_2D, 1, 0, 0, 0,
                               tex[1], GL_TEXTURE_2D, 1, 0, 0, 0,
                               half, half, 1);
        glBindTexture(GL_TEXTURE_2D, tex[1]);
        memset(got, 0, sizeof(got));
        glGetTexImage(GL_TEXTURE_2D, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
        const GLenum error = glGetError();
        unsigned differ = 0;
        for (size_t i = 0; i < sizeof(got); ++i)
            if (got[i] != src_bytes[i]) ++differ;
        fprintf(stderr, "INFO mip-level differ=%u err=0x%04x\n", differ,
                (unsigned)error);
        result("mip-level", have && error == GL_NO_ERROR && differ == 0);
        glDeleteTextures(2, tex);
    }

    /* Case 5: two different eight-byte formats. No conversion may happen --
     * the bits arrive as they left, which is what separates a copy from a
     * blit through a shader. */
    {
        uint32_t src_words[SIDE * SIDE * 2];
        uint32_t got_words[SIDE * SIDE * 2];
        for (unsigned i = 0; i < SIDE * SIDE * 2; ++i)
            src_words[i] = 0x80000000u | (i * 2654435761u);
        GLuint src_tex = 0, dst_tex = 0;
        glGenTextures(1, &src_tex);
        glBindTexture(GL_TEXTURE_2D, src_tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RG32UI, SIDE, SIDE);
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIDE, SIDE, GL_RG_INTEGER,
                        GL_UNSIGNED_INT, src_words);
        glGenTextures(1, &dst_tex);
        glBindTexture(GL_TEXTURE_2D, dst_tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA16UI, SIDE, SIDE);
        if (have)
            glCopyImageSubData(src_tex, GL_TEXTURE_2D, 0, 0, 0, 0,
                               dst_tex, GL_TEXTURE_2D, 0, 0, 0, 0,
                               SIDE, SIDE, 1);
        glBindTexture(GL_TEXTURE_2D, dst_tex);
        memset(got_words, 0, sizeof(got_words));
        glGetTexImage(GL_TEXTURE_2D, 0, GL_RGBA_INTEGER, GL_UNSIGNED_SHORT,
                      got_words);
        const GLenum error = glGetError();
        unsigned differ = 0;
        for (unsigned i = 0; i < SIDE * SIDE * 2; ++i)
            if (got_words[i] != src_words[i]) ++differ;
        fprintf(stderr, "INFO class-8byte differ=%u err=0x%04x\n", differ,
                (unsigned)error);
        result("class-8byte", have && error == GL_NO_ERROR && differ == 0);
        glDeleteTextures(1, &src_tex);
        glDeleteTextures(1, &dst_tex);
    }

    /* Case 6: the pairing the cap is named for. A 16x16 DXT1 image is 4x4
     * blocks of eight bytes, which is a 4x4 RG32UI image; copying one to the
     * other and back has to return the same blocks. */
    {
        unsigned char blocks[(SIDE / 4) * (SIDE / 4) * 8];
        unsigned char got[(SIDE / 4) * (SIDE / 4) * 8];
        fill(blocks, sizeof(blocks), 6);
        int ran = 0;
        unsigned differ = 0xffff;
        GLenum error = GL_NO_ERROR;
        if (have && have_s3tc) {
            GLuint cmp = 0, plain = 0, back = 0;
            glGenTextures(1, &cmp);
            glBindTexture(GL_TEXTURE_2D, cmp);
            glTexStorage2D(GL_TEXTURE_2D, 1,
                           GL_COMPRESSED_RGB_S3TC_DXT1_EXT, SIDE, SIDE);
            glCompressedTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIDE, SIDE,
                                      GL_COMPRESSED_RGB_S3TC_DXT1_EXT,
                                      sizeof(blocks), blocks);
            glGenTextures(1, &plain);
            glBindTexture(GL_TEXTURE_2D, plain);
            glTexStorage2D(GL_TEXTURE_2D, 1, GL_RG32UI, SIDE / 4, SIDE / 4);
            glGenTextures(1, &back);
            glBindTexture(GL_TEXTURE_2D, back);
            glTexStorage2D(GL_TEXTURE_2D, 1,
                           GL_COMPRESSED_RGB_S3TC_DXT1_EXT, SIDE, SIDE);
            if (glGetError() == GL_NO_ERROR) {
                ran = 1;
                /* The extent is in *texels of the compressed image*, which is
                 * four times the plain image's -- the specification says so,
                 * and a driver that takes the plain extent copies a sixteenth
                 * of the data. */
                glCopyImageSubData(cmp, GL_TEXTURE_2D, 0, 0, 0, 0,
                                   plain, GL_TEXTURE_2D, 0, 0, 0, 0,
                                   SIDE, SIDE, 1);
                glCopyImageSubData(plain, GL_TEXTURE_2D, 0, 0, 0, 0,
                                   back, GL_TEXTURE_2D, 0, 0, 0, 0,
                                   SIDE / 4, SIDE / 4, 1);
                glBindTexture(GL_TEXTURE_2D, back);
                memset(got, 0, sizeof(got));
                glGetCompressedTexImage(GL_TEXTURE_2D, 0, got);
                error = glGetError();
                differ = 0;
                for (size_t i = 0; i < sizeof(got); ++i)
                    if (got[i] != blocks[i]) ++differ;
            }
            glDeleteTextures(1, &cmp);
            glDeleteTextures(1, &plain);
            glDeleteTextures(1, &back);
        }
        fprintf(stderr, "INFO compressed ran=%d differ=%u err=0x%04x\n", ran,
                differ, (unsigned)error);
        /* Skipped rather than failed where the compressed format is absent:
         * S3TC is an optional WebGPU feature and the rest of the extension
         * does not depend on it. */
        if (!have_s3tc) {
            printf("CASE compressed SKIP\n");
        } else {
            result("compressed",
                   have && ran && error == GL_NO_ERROR && differ == 0);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
