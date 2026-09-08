/* The promise GL_MIN_MAP_BUFFER_ALIGNMENT makes, checked against what a map
 * actually returns.
 *
 * An alignment cap and the allocation behind it are one fact in two places,
 * which is the shape that drifts: this driver advertised 64 and allocated the
 * block a map returns with CALLOC, which glibc aligns to 16 on x86-64. An
 * application that stores through the pointer with aligned SIMD is entitled to
 * the number the cap gave it, and nothing else in the tree checks it -- the
 * CTS's map_buffer_alignment has a `query` case that reads the number and a
 * `functional` case that walks sixteen storage-flag combinations, so a plain
 * "is the pointer aligned" answer is not what either of them reports.
 *
 *   map_buffer_aligned    glMapBuffer's pointer is a multiple of the cap
 *   map_range_aligned     glMapBufferRange at offset zero likewise
 *   map_buffer_data       and the bytes read back through it are the buffer's
 *   map_range_offset      including through a range mapped at cap-1, the
 *                         offset the CTS uses, where the base alignment does
 *                         not help and only the addressing can be right
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

int main(void)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(dpy, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 1; }
    static const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                                       EGL_NONE};
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, NULL);
    if (ctx == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 1; }
    static const EGLint surf_attrs[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);

    GLint align = 0;
    glGetIntegerv(GL_MIN_MAP_BUFFER_ALIGNMENT, &align);
    /* GL requires at least 64; a driver reporting less is broken before any
     * of the cases below can mean anything. */
    if (align < 64) {
        fprintf(stderr, "INFO GL_MIN_MAP_BUFFER_ALIGNMENT = %d, GL wants 64\n",
                align);
        result("min_alignment_at_least_64", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 0;
    }

    const unsigned size = 2u * (unsigned)align;
    unsigned char *reference = malloc(size);
    for (unsigned i = 0; i < size; ++i)
        reference[i] = (unsigned char)(i * 7u + 1u);

    GLuint buffer = 0;
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)size, reference, GL_STATIC_DRAW);

    unsigned char *whole = glMapBuffer(GL_ARRAY_BUFFER, GL_READ_ONLY);
    const size_t whole_mod =
        whole ? (size_t)((uintptr_t)whole % (unsigned)align) : 1u;
    const int whole_data = whole && memcmp(whole, reference, size) == 0;
    fprintf(stderr, "INFO glMapBuffer ptr%%%d=%zu data_ok=%d\n", align,
            whole_mod, whole_data);
    result("map_buffer_aligned", whole_mod == 0);
    result("map_buffer_data", whole_data);
    if (whole) glUnmapBuffer(GL_ARRAY_BUFFER);

    unsigned char *ranged =
        glMapBufferRange(GL_ARRAY_BUFFER, 0, (GLsizeiptr)size, GL_MAP_READ_BIT);
    const size_t ranged_mod =
        ranged ? (size_t)((uintptr_t)ranged % (unsigned)align) : 1u;
    fprintf(stderr, "INFO glMapBufferRange(0) ptr%%%d=%zu\n", align,
            ranged_mod);
    result("map_range_aligned", ranged_mod == 0);
    if (ranged) glUnmapBuffer(GL_ARRAY_BUFFER);

    /* cap - 1: the offset the CTS uses, chosen so the base alignment cannot
     * carry the answer. Only the addressing can be right here. */
    const unsigned offset = (unsigned)align - 1u;
    unsigned char *shifted =
        glMapBufferRange(GL_ARRAY_BUFFER, (GLintptr)offset,
                         (GLsizeiptr)(size - offset), GL_MAP_READ_BIT);
    const int shifted_data =
        shifted && memcmp(shifted, reference + offset, size - offset) == 0;
    fprintf(stderr, "INFO glMapBufferRange(%u) data_ok=%d err=0x%04x\n", offset,
            shifted_data, (unsigned)glGetError());
    result("map_range_offset", shifted_data);
    if (shifted) glUnmapBuffer(GL_ARRAY_BUFFER);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
