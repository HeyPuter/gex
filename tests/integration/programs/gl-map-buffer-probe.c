/* glMapBufferRange keeps what it should: a write-only map without an
 * invalidate bit leaves unwritten bytes as they were, and a map with
 * GL_MAP_FLUSH_EXPLICIT_BIT changes only the ranges flushed (Unity's ring
 * buffers). gex-gl's guest shadow used to upload zeros over both. A 40 MiB
 * buffer, larger than the ring's 32 MiB reply area, reads back whole and
 * keeps its unwritten bytes through a write map (both read in parts). Prints
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

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
