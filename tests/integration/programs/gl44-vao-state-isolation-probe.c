/* Each vertex array object has its own attribute state.
 *
 * KHR-GL44.vertex_attrib_binding.basic-state4 creates a *fresh* VAO and
 * verifies every attribute is at its default. It fails in the browser only
 * after basic-state1..3 have run -- it passes alone, and passes with the
 * pipeline cache disabled -- which is the shape of state leaking from one VAO
 * into the next rather than of any single call being wrong.
 *
 * So this sets state on one VAO, makes another, and reads the second's back.
 * Each attribute is checked separately so a partial leak says which.
 *
 * The field that actually leaked is **GL_VERTEX_ATTRIB_ARRAY_POINTER**, and
 * the first version of this probe checked divisors and enabled flags and
 * missed it -- it passed on both paths while the CTS case went on failing.
 * The pointer is the one piece of attribute state the guest has to answer
 * from its own records, because a client array's pointer is guest memory the
 * host is never told about, so it is exactly the piece most likely to be kept
 * in the wrong place. Checked first here for that reason.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-42s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

#define ATTRS 8

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    /* A buffer, so glVertexAttribPointer's last argument is an offset rather
       than a client pointer -- which is what the CTS case does, and what the
       guest records either way. */
    GLuint vbo = 0; glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, 4096, NULL, GL_STATIC_DRAW);

    /* The first VAO, given a distinctive divisor and pointer on every
       attribute. */
    GLuint first = 0; glGenVertexArrays(1, &first);
    glBindVertexArray(first);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    for (GLuint i = 0; i < ATTRS; ++i) {
        glVertexAttribDivisor(i, i + 7);
        glEnableVertexAttribArray(i);
        glVertexAttribPointer(i, 4, GL_FLOAT, GL_FALSE, 0,
                              (const void *)(uintptr_t)((i + 1) * 64));
    }
    eq("setting_the_first_vao_is_clean", drain(), GL_NO_ERROR);
    /* Read one back to be sure it took, so a leak is distinguishable from
       nothing having been written at all. */
    GLint wrote = -1;
    glGetVertexAttribiv(3, GL_VERTEX_ATTRIB_ARRAY_DIVISOR, &wrote);
    drain();
    eq("first_vao_kept_its_divisor", wrote, 3 + 7);

    /* A second, fresh VAO. Every attribute must read back at its default. */
    GLuint second = 0; glGenVertexArrays(1, &second);
    glBindVertexArray(second);
    eq("binding_a_fresh_vao_is_clean", drain(), GL_NO_ERROR);
    int leaked_divisor = 0, leaked_enabled = 0, leaked_pointer = 0;
    for (GLuint i = 0; i < ATTRS; ++i) {
        GLint divisor = -1, enabled = -1;
        void *ptr = (void *)(uintptr_t)0xdeadu;
        glGetVertexAttribPointerv(i, GL_VERTEX_ATTRIB_ARRAY_POINTER, &ptr);
        if (ptr != NULL) {
            ++leaked_pointer;
            fprintf(stderr, "INFO   attr %u pointer=%llu want 0\n", i,
                    (unsigned long long)(uintptr_t)ptr);
        }
        glGetVertexAttribiv(i, GL_VERTEX_ATTRIB_ARRAY_DIVISOR, &divisor);
        glGetVertexAttribiv(i, GL_VERTEX_ATTRIB_ARRAY_ENABLED, &enabled);
        if (divisor != 0) {
            ++leaked_divisor;
            fprintf(stderr, "INFO   attr %u divisor=%d want 0\n", i, divisor);
        }
        if (enabled != 0) {
            ++leaked_enabled;
            fprintf(stderr, "INFO   attr %u enabled=%d want 0\n", i, enabled);
        }
    }
    drain();
    eq("fresh_vao_pointers_are_null", leaked_pointer, 0);
    eq("fresh_vao_divisors_are_zero", leaked_divisor, 0);
    eq("fresh_vao_arrays_are_disabled", leaked_enabled, 0);

    /* And the binding divisors, which glVertexBindingDivisor writes and GL
       also keeps per VAO. */
    glBindVertexArray(first);
    for (GLuint i = 0; i < ATTRS; ++i)
        glVertexBindingDivisor(i, i + 3);
    drain();
    glBindVertexArray(second);
    int leaked_binding = 0;
    for (GLuint i = 0; i < ATTRS; ++i) {
        GLint divisor = -1;
        glGetIntegeri_v(GL_VERTEX_BINDING_DIVISOR, i, &divisor);
        if (divisor != 0) {
            ++leaked_binding;
            fprintf(stderr, "INFO   binding %u divisor=%d want 0\n", i,
                    divisor);
        }
    }
    drain();
    eq("fresh_vao_binding_divisors_are_zero", leaked_binding, 0);

    /* Going back to the first must still find what it was given: an
       isolation bug can lose state as easily as leak it.
       The expected value is i + 3, not the i + 7 set earlier: from GL 4.3
       glVertexAttribDivisor and glVertexBindingDivisor write the *same*
       state -- an attribute's divisor is its binding's -- so the second call
       above replaced the first. The reference is what said so; expecting
       i + 7 here reported eight leaks that were the aliasing working
       correctly. */
    glBindVertexArray(first);
    int lost = 0;
    for (GLuint i = 0; i < ATTRS; ++i) {
        GLint divisor = -1;
        glGetVertexAttribiv(i, GL_VERTEX_ATTRIB_ARRAY_DIVISOR, &divisor);
        if (divisor != (GLint)(i + 3)) {
            ++lost;
            fprintf(stderr, "INFO   attr %u divisor=%d want %u\n", i, divisor,
                    i + 3);
        }
    }
    drain();
    eq("the_first_vao_still_has_its_state", lost, 0);

    /* And its pointers, so a fix cannot pass by clearing everything on
       every bind. */
    int lost_pointers = 0;
    for (GLuint i = 0; i < ATTRS; ++i) {
        void *ptr = NULL;
        glGetVertexAttribPointerv(i, GL_VERTEX_ATTRIB_ARRAY_POINTER, &ptr);
        if ((uintptr_t)ptr != (uintptr_t)((i + 1) * 64)) {
            ++lost_pointers;
            fprintf(stderr, "INFO   attr %u pointer=%llu want %llu\n", i,
                    (unsigned long long)(uintptr_t)ptr,
                    (unsigned long long)((i + 1) * 64));
        }
    }
    drain();
    eq("the_first_vao_still_has_its_pointers", lost_pointers, 0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
