/* glMapBufferRange keeps what it should: a write-only map without an
 * invalidate bit leaves unwritten bytes as they were, and a map with
 * GL_MAP_FLUSH_EXPLICIT_BIT changes only the ranges flushed (Unity's ring
 * buffers). gex-gl's guest shadow used to upload zeros over both. Prints
 * SUMMARY cases=N passed=N failed=0 when right; runs on native Mesa too.
 *
 *   cc -o probe gl-map-buffer-probe.c -lEGL -lGL       (native reference)
 *   gexc run --gl --env LD_LIBRARY_PATH=GUEST_GL probe (guest/gl/build.sh) */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
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

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
