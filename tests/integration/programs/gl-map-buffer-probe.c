/* glMapBufferRange keeps what it should: a write-only map without an
 * invalidate bit leaves unwritten bytes as they were, and a map with
 * GL_MAP_FLUSH_EXPLICIT_BIT changes only the ranges flushed (Unity's ring
 * buffers). gex-gl's guest shadow used to upload zeros over both. A 40 MiB
 * buffer, larger than the ring's 32 MiB reply area, reads back whole and
 * keeps its unwritten bytes through a write map (both read in parts). The
 * guest keeps a copy of a mapped buffer's contents and uploads only what a
 * write map changed: the copy must follow glBufferSubData and must not
 * survive a GPU write (glCopyBufferSubData). A 4096x4096 RGBA texture
 * (64 MiB, larger than the command area) uploads whole, with
 * GL_UNPACK_SKIP_ROWS and GL_UNPACK_ROW_LENGTH set; so do 3D textures
 * (64 MiB in whole images; 128 MiB of 64 MiB RGBA32F images, in row bands) and a 64 MiB DXT5 image,
 * each read back through a pixel pack buffer. A 64 MiB ASTC upload (not
 * split) must leave one "gexgl: dropped opcode" line on stderr under gex,
 * however often it is tried. Prints
 * SUMMARY cases=N passed=N failed=0 when right; runs on native Mesa too.
 *
 *   cc -o probe gl-map-buffer-probe.c -lEGL -lGL       (native reference)
 *   gexc run --gl --env LD_LIBRARY_PATH=GUEST_GL probe (guest/gl/build.sh) */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases, passed;

static void check(const char *name, GLuint buf, const unsigned char *want) {
    unsigned char got[64];
    glBindBuffer(GL_ARRAY_BUFFER, buf);
    glGetBufferSubData(GL_ARRAY_BUFFER, 0, sizeof got, got);
    int ok = memcmp(got, want, sizeof got) == 0;
    cases++;
    passed += ok;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
    if (!ok) {
        for (int i = 0; i < 64; i++) printf("%02x%s", got[i], i % 16 == 15 ? "\n" : " ");
    }
}

static GLuint fresh(unsigned char *shadow) {
    GLuint b;
    glGenBuffers(1, &b);
    glBindBuffer(GL_ARRAY_BUFFER, b);
    for (int i = 0; i < 64; i++) shadow[i] = (unsigned char)(0xa0 + i);
    glBufferData(GL_ARRAY_BUFFER, 64, shadow, GL_DYNAMIC_DRAW);
    return b;
}

/* 40 MiB: more than one reply can carry, so gexgl reads it in parts. */
#define BIG ((size_t)40 << 20)

static unsigned char big_byte(size_t i) { return (unsigned char)(i * 131u + (i >> 20) * 7u + 1u); }

static void check_big(const char *name, GLuint buf, size_t poke, size_t poke_len, unsigned char value) {
    unsigned char *got = malloc(BIG);
    size_t bad = BIG;
    memset(got, 0, BIG);
    glBindBuffer(GL_ARRAY_BUFFER, buf);
    glGetBufferSubData(GL_ARRAY_BUFFER, 0, (GLsizeiptr)BIG, got);
    for (size_t i = 0; i < BIG && bad == BIG; i++) {
        unsigned char want = (i >= poke && i < poke + poke_len) ? value : big_byte(i);
        if (got[i] != want) bad = i;
    }
    cases++;
    passed += bad == BIG;
    printf("CASE %s %s\n", name, bad == BIG ? "PASS" : "FAIL");
    if (bad != BIG) printf("first difference at byte %zu: %02x\n", bad, got[bad]);
    free(got);
}

/* KiB this process has mapped (the sum of /proc/self/maps), or -1. */
static long vmdata_kib(void) {
    FILE *f = fopen("/proc/self/maps", "r");
    char line[512];
    unsigned long lo, hi;
    long kib = f ? 0 : -1;
    while (f && fgets(line, sizeof line, f))
        if (sscanf(line, "%lx-%lx", &lo, &hi) == 2) kib += (long)((hi - lo) >> 10);
    if (f) fclose(f);
    return kib;
}

static GLuint fresh_big(void) {
    unsigned char *data = malloc(BIG);
    for (size_t i = 0; i < BIG; i++) data[i] = big_byte(i);
    GLuint b;
    glGenBuffers(1, &b);
    glBindBuffer(GL_ARRAY_BUFFER, b);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)BIG, data, GL_DYNAMIC_DRAW);
    free(data);
    return b;
}

/* 4096x4096 RGBA from client memory: rows 10.. of a 4100-pixel-wide image. */
#define TEX 4096
#define TEX_ROW 4100
static unsigned tex_texel(size_t x, size_t y) { return (unsigned)(x * 2654435761u ^ y * 40503u) | 0xff000000u; }

static void check_big_texture(const char *name, int sub) {
    unsigned *src = malloc((size_t)TEX_ROW * (TEX + 10) * 4);
    for (size_t y = 0; y < TEX + 10; y++)
        for (size_t x = 0; x < TEX_ROW; x++) src[y * TEX_ROW + x] = tex_texel(x, y);
    GLuint t, fb;
    glGenTextures(1, &t);
    glBindTexture(GL_TEXTURE_2D, t);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, TEX_ROW);
    glPixelStorei(GL_UNPACK_SKIP_ROWS, 10);
    if (sub) {
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, TEX, TEX, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, TEX, TEX, GL_RGBA, GL_UNSIGNED_BYTE, src);
    } else {
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, TEX, TEX, 0, GL_RGBA, GL_UNSIGNED_BYTE, src);
    }
    glPixelStorei(GL_UNPACK_ROW_LENGTH, 0);
    glPixelStorei(GL_UNPACK_SKIP_ROWS, 0);
    glGenFramebuffers(1, &fb);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, t, 0);
    /* Rows on both sides of every possible band edge are a few spans:
       read 8 rows of 64 pixels at a spread of heights. */
    int ok = 1;
    unsigned got[8 * 64];
    for (int y = 0; y < TEX && ok; y += 509) {
        int x = (y * 7) % (TEX - 64);
        glReadPixels(x, y, 64, 8, GL_RGBA, GL_UNSIGNED_BYTE, got);
        for (int r = 0; r < 8 && ok; r++)
            for (int c = 0; c < 64 && ok; c++)
                if (got[r * 64 + c] != tex_texel((size_t)(x + c), (size_t)(y + r + 10))) {
                    ok = 0;
                    printf("texel (%d,%d): %08x, want %08x\n", x + c, y + r, got[r * 64 + c],
                           tex_texel((size_t)(x + c), (size_t)(y + r + 10)));
                }
    }
    glReadPixels(TEX - 64, TEX - 8, 64, 8, GL_RGBA, GL_UNSIGNED_BYTE, got);
    for (int i = 0; i < 8 * 64 && ok; i++)
        if (got[i] != tex_texel((size_t)(TEX - 64 + i % 64), (size_t)(TEX - 8 + i / 64 + 10))) ok = 0;
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glDeleteFramebuffers(1, &fb);
    glDeleteTextures(1, &t);
    free(src);
    cases++; passed += ok;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* Read a texture level back whole through a pack buffer (a reply could
   not carry it) and compare it with WANT. */
static int pack_readback_equals(GLenum target, int compressed, GLenum format, GLenum type,
                                const unsigned char *want, size_t len) {
    GLuint pb;
    glGenBuffers(1, &pb);
    glBindBuffer(GL_PIXEL_PACK_BUFFER, pb);
    glBufferData(GL_PIXEL_PACK_BUFFER, (GLsizeiptr)len, NULL, GL_STREAM_READ);
    if (compressed) glGetCompressedTexImage(target, 0, NULL);
    else glGetTexImage(target, 0, format, type, NULL);
    unsigned char *got = malloc(len);
    memset(got, 0, len);
    glGetBufferSubData(GL_PIXEL_PACK_BUFFER, 0, (GLsizeiptr)len, got);
    glBindBuffer(GL_PIXEL_PACK_BUFFER, 0);
    glDeleteBuffers(1, &pb);
    size_t bad = len;
    for (size_t i = 0; i < len && bad == len; i++)
        if (got[i] != want[i]) bad = i;
    if (bad != len) printf("first difference at byte %zu: %02x, want %02x\n", bad, got[bad], want[bad]);
    free(got);
    return bad == len;
}

/* A W x H x D 3D texture (RGBA8, or RGBA32F when FLOATS) from client
   memory with ROW_LENGTH W+6, IMAGE_HEIGHT H+3, SKIP_ROWS 3 and
   SKIP_IMAGES 1. Every word is an ordinary float's bits, so RGBA32F keeps it. */
static void check_big_3d(const char *name, int w, int h, int d, int floats) {
    size_t px = floats ? 16 : 4;
    size_t rl = (size_t)w + 6, ih = (size_t)h + 3;
    size_t src_len = rl * ih * ((size_t)d + 1) * px;
    unsigned *src = malloc(src_len);
    for (size_t i = 0; i < src_len / 4; i++) {
        float f = (float)((i * 2654435761u) & 0xffff) + 0.5f;
        memcpy(&src[i], &f, 4);
    }
    unsigned char *want = malloc((size_t)w * h * d * px);
    for (int z = 0; z < d; z++)
        for (int y = 0; y < h; y++)
            memcpy(want + (((size_t)z * h + y) * w) * px,
                   (unsigned char *)src + ((size_t)(z + 1) * ih + (size_t)(y + 3)) * rl * px, (size_t)w * px);
    GLuint t;
    glGenTextures(1, &t);
    glBindTexture(GL_TEXTURE_3D, t);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, (int)rl);
    glPixelStorei(GL_UNPACK_IMAGE_HEIGHT, (int)ih);
    glPixelStorei(GL_UNPACK_SKIP_ROWS, 3);
    glPixelStorei(GL_UNPACK_SKIP_IMAGES, 1);
    glTexImage3D(GL_TEXTURE_3D, 0, floats ? GL_RGBA32F : GL_RGBA8, w, h, d, 0, GL_RGBA,
                 floats ? GL_FLOAT : GL_UNSIGNED_BYTE, src);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, 0);
    glPixelStorei(GL_UNPACK_IMAGE_HEIGHT, 0);
    glPixelStorei(GL_UNPACK_SKIP_ROWS, 0);
    glPixelStorei(GL_UNPACK_SKIP_IMAGES, 0);
    int ok = pack_readback_equals(GL_TEXTURE_3D, 0, GL_RGBA, floats ? GL_FLOAT : GL_UNSIGNED_BYTE, want,
                                  (size_t)w * h * d * px);
    glDeleteTextures(1, &t);
    free(src);
    free(want);
    cases++; passed += ok;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* 8192x8192 DXT5: 64 MiB of blocks. */
static void check_big_dxt5(void) {
    size_t len = (size_t)2048 * 2048 * 16;
    unsigned char *blocks = malloc(len);
    for (size_t i = 0; i < len; i++) blocks[i] = (unsigned char)(i * 167u + (i >> 16));
    GLuint t;
    glGenTextures(1, &t);
    glBindTexture(GL_TEXTURE_2D, t);
    glCompressedTexImage2D(GL_TEXTURE_2D, 0, 0x83F3 /* GL_COMPRESSED_RGBA_S3TC_DXT5_EXT */,
                           8192, 8192, 0, (GLsizei)len, blocks);
    int ok = pack_readback_equals(GL_TEXTURE_2D, 1, 0, 0, blocks, len);
    glDeleteTextures(1, &t);
    free(blocks);
    cases++; passed += ok;
    printf("CASE big_dxt5 %s\n", ok ? "PASS" : "FAIL");
}

int main(void) {
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "FATAL egl_initialize error=0x%x\n", eglGetError());
        return 2;
    }
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count) || config_count < 1) return 2;
    const EGLint context_attributes[] = {EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 4, EGL_NONE};
    const EGLint surface_attributes[] = {EGL_WIDTH, 16, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE || !eglMakeCurrent(display, surface, surface, context))
        return 2;

    unsigned char want[64];

    /* Write-only, no invalidate: bytes 16..47 mapped, only 20..23 written. */
    GLuint a = fresh(want);
    unsigned char *p = glMapBufferRange(GL_ARRAY_BUFFER, 16, 32, GL_MAP_WRITE_BIT);
    if (p) memset(p + 4, 0x11, 4);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    memset(want + 20, 0x11, 4);
    check("write_keeps_unwritten", a, want);

    /* Flush explicit: map 0..63, write 8..15 and 40..47, flush only 8..15. */
    GLuint b = fresh(want);
    p = glMapBufferRange(GL_ARRAY_BUFFER, 0, 64,
                         GL_MAP_WRITE_BIT | GL_MAP_FLUSH_EXPLICIT_BIT | GL_MAP_UNSYNCHRONIZED_BIT);
    if (p) {
        memset(p + 8, 0x22, 8);
        glFlushMappedBufferRange(GL_ARRAY_BUFFER, 8, 8);
    }
    glUnmapBuffer(GL_ARRAY_BUFFER);
    memset(want + 8, 0x22, 8);
    check("flush_explicit_only_flushed", b, want);

    /* Invalidate range: the whole mapped range written, as the app must. */
    GLuint c = fresh(want);
    p = glMapBufferRange(GL_ARRAY_BUFFER, 32, 16, GL_MAP_WRITE_BIT | GL_MAP_INVALIDATE_RANGE_BIT);
    if (p) memset(p, 0x33, 16);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    memset(want + 32, 0x33, 16);
    check("invalidate_range_written", c, want);

    /* 40 MiB read back whole. */
    GLuint d = fresh_big();
    check_big("big_readback", d, 0, 0, 0);

    /* 40 MiB write-only map, 4 KiB written near the end: the rest stays. */
    GLuint e = fresh_big();
    p = glMapBuffer(GL_ARRAY_BUFFER, GL_WRITE_ONLY);
    if (p) memset(p + BIG - 8192, 0x44, 4096);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    check_big("big_write_map_keeps_rest", e, BIG - 8192, 4096, 0x44);

    /* A second write map of the same 40 MiB buffer: both writes stay. */
    p = glMapBufferRange(GL_ARRAY_BUFFER, 4096, 8192, GL_MAP_WRITE_BIT);
    if (p) memset(p + 100, 0x45, 16);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    {
        unsigned char got[16], w2[16], w1[16];
        memset(w2, 0x45, 16);
        glGetBufferSubData(GL_ARRAY_BUFFER, 4096 + 100, 16, got);
        int ok = memcmp(got, w2, 16) == 0;
        glGetBufferSubData(GL_ARRAY_BUFFER, (GLintptr)(BIG - 8192), 16, w1);
        memset(w2, 0x44, 16);
        ok = ok && memcmp(w1, w2, 16) == 0;
        cases++; passed += ok;
        printf("CASE big_second_write_map %s\n", ok ? "PASS" : "FAIL");
    }

    /* A 1 MiB read map in the middle of a 40 MiB buffer shows its bytes,
       and (GEXGL_PROBE_VMDATA=1, gex's run) maps about the 1 MiB shadow:
       gexgl used to keep a whole-size copy of the buffer besides it, which a
       32-bit W3 process (wined3d's 64 MiB chunks) has no room for. */
    {
        GLuint g = fresh_big();
        long before = vmdata_kib();
        const unsigned char *r = glMapBufferRange(GL_ARRAY_BUFFER, 8 << 20, 1 << 20, GL_MAP_READ_BIT);
        long during = vmdata_kib();
        int ok = r != NULL;
        for (size_t i = 0; ok && i < (1u << 20); i++) ok = r[i] == big_byte((8u << 20) + i);
        glUnmapBuffer(GL_ARRAY_BUFFER);
        if (getenv("GEXGL_PROBE_VMDATA") && ok && (before < 0 || during < 0 || during - before > 4096)) {
            printf("map of 1 MiB of a 40 MiB buffer took %ld KiB of address space\n", during - before);
            ok = 0;
        }
        glDeleteBuffers(1, &g);
        cases++; passed += ok;
        printf("CASE big_range_read_map_no_copy %s\n", ok ? "PASS" : "FAIL");
    }

    /* glBufferSubData after a map: the next write map keeps its bytes. */
    GLuint f = fresh(want);
    p = glMapBufferRange(GL_ARRAY_BUFFER, 0, 64, GL_MAP_WRITE_BIT);
    if (p) memset(p, 0x50, 4);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    memset(want, 0x50, 4);
    unsigned char sub[8];
    memset(sub, 0x55, 8);
    glBufferSubData(GL_ARRAY_BUFFER, 8, 8, sub);
    memset(want + 8, 0x55, 8);
    p = glMapBufferRange(GL_ARRAY_BUFFER, 32, 32, GL_MAP_WRITE_BIT);
    if (p) memset(p + 8, 0x56, 4);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    memset(want + 40, 0x56, 4);
    check("sub_data_after_map_kept", f, want);

    /* A GPU copy into a mapped-before buffer: a read map and the next
       write map both see the copied bytes. */
    unsigned char other[64];
    GLuint src = fresh(other);
    memset(other, 0x66, 64);
    glBufferSubData(GL_ARRAY_BUFFER, 0, 64, other);
    GLuint g = fresh(want);
    p = glMapBufferRange(GL_ARRAY_BUFFER, 0, 64, GL_MAP_WRITE_BIT);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    glBindBuffer(GL_COPY_READ_BUFFER, src);
    glBindBuffer(GL_COPY_WRITE_BUFFER, g);
    glCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER, 0, 0, 16);
    memset(want, 0x66, 16);
    glBindBuffer(GL_ARRAY_BUFFER, g);
    p = glMapBufferRange(GL_ARRAY_BUFFER, 0, 64, GL_MAP_READ_BIT);
    int seen = p && memcmp(p, want, 64) == 0;
    glUnmapBuffer(GL_ARRAY_BUFFER);
    cases++; passed += seen;
    printf("CASE read_map_sees_gpu_copy %s\n", seen ? "PASS" : "FAIL");
    GLuint h = fresh(want);
    p = glMapBufferRange(GL_ARRAY_BUFFER, 0, 64, GL_MAP_WRITE_BIT);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    glBindBuffer(GL_COPY_WRITE_BUFFER, h);
    glCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER, 0, 0, 16);
    memset(want, 0x66, 16);
    glBindBuffer(GL_ARRAY_BUFFER, h);
    p = glMapBufferRange(GL_ARRAY_BUFFER, 0, 64, GL_MAP_WRITE_BIT);
    if (p) memset(p + 60, 0x67, 4);
    glUnmapBuffer(GL_ARRAY_BUFFER);
    memset(want + 60, 0x67, 4);
    check("write_map_keeps_gpu_copy", h, want);

    check_big_texture("big_tex_image", 0);
    check_big_texture("big_tex_sub_image", 1);

    check_big_3d("big_3d_images", 1024, 1024, 16, 0);
    check_big_3d("big_3d_rows", 2048, 2048, 2, 1); /* 64 MiB images: in row bands */
    check_big_dxt5();

    /* An upload gexgl does not split (ASTC 4x4, 64 MiB), twice: gexgl says
       so once. Not a case (natively it may or may not be supported). */
    {
        size_t len = (size_t)2048 * 2048 * 16;
        unsigned char *astc = calloc(1, len);
        GLuint ta;
        glGenTextures(1, &ta);
        glBindTexture(GL_TEXTURE_2D, ta);
        for (int i = 0; i < 2; i++)
            glCompressedTexImage2D(GL_TEXTURE_2D, 0, 0x93B0 /* GL_COMPRESSED_RGBA_ASTC_4x4_KHR */,
                                   8192, 8192, 0, (GLsizei)len, astc);
        glFinish();
        while (glGetError() != GL_NO_ERROR) {}
        glDeleteTextures(1, &ta);
        free(astc);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
