/* Binding a name the application never generated.
 *
 * GL is explicit: in a core profile, handing one of these entry points a
 * non-zero name that was never returned by the matching glGen* is
 * GL_INVALID_OPERATION. The transport's `lookup` mapped an unknown guest name
 * to zero, and zero is a legal *unbind*, so the error became a silent success.
 * The host has had lookup_bindable for this since
 * KHR-GL32.buffer_objects.gen_buffers -- it substitutes a name the host's own
 * GL has never generated, so GL raises the error itself rather than the
 * transport guessing which one -- and it was used at exactly one site,
 * glBindBuffer, and none of the generated ones.
 *
 * KHR-GL44.vertex_attrib_binding.negative-bindVertexBuffer failed on this in
 * the browser and passed natively, which is the interesting part: **the native
 * reference is a compatibility context**, where binding an ungenerated name is
 * legal and creates the object. Only the core-profile browser owed the error,
 * so only the browser was wrong, and the native run could not see it.
 *
 * This probe therefore asks EGL for a *core* profile explicitly, so both sides
 * are held to the same rule and the reference is a real oracle. Without that
 * it would assert the compatibility answer natively and the core answer in the
 * browser, and report a difference that is the profile rather than a defect.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include "gl-procs.h"

static int cases, passed;

static GLenum drain(void) {
    GLenum last = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) last = e;
    return last;
}

/* The specification says GL_INVALID_OPERATION throughout. "An error, any
   error" would pass on a driver that raised the wrong one, and the point here
   is that *nothing* was raised, so the exact code is worth asserting -- and
   the reference agrees on it, which is the test that matters. */
static void check(const char *name, GLenum got) {
    ++cases;
    const int ok = got == GL_INVALID_OPERATION;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-38s got=0x%04x want=0x0502\n", name,
            (unsigned)got);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* A name no glGen* has returned. Large enough that a driver handing out
   sequential names will not reach it during this probe. */
#define UNGENERATED 0x4000u

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
    /* Core, and 4.4 so the whole bind family exists. */
    const EGLint xa[] = { 0x3098 /* EGL_CONTEXT_MAJOR_VERSION */, 4,
                          0x30FB /* EGL_CONTEXT_MINOR_VERSION */, 4,
                          0x30FD /* EGL_CONTEXT_OPENGL_PROFILE_MASK */,
                          0x00000001 /* CORE_PROFILE_BIT */,
                          EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, xa);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    if (gex_load_procs() != 0) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 3;
    }
    /* A core profile needs a vertex array object bound before the vertex
       entry points will do anything but raise. */
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    drain();

    glBindBuffer(GL_ARRAY_BUFFER, UNGENERATED);
    check("bind_buffer", drain());
    glBindBuffer(GL_ARRAY_BUFFER, 0);
    drain();

    glBindVertexBuffer(0, UNGENERATED, 0, 16);
    check("bind_vertex_buffer", drain());

    const GLuint one[1] = { UNGENERATED };
    glBindVertexBuffers(0, 1, one, (const GLintptr[]){0},
                        (const GLsizei[]){16});
    check("bind_vertex_buffers", drain());

    glBindBufferRange(GL_UNIFORM_BUFFER, 0, UNGENERATED, 0, 16);
    check("bind_buffer_range", drain());

    glBindBuffersBase(GL_UNIFORM_BUFFER, 0, 1, one);
    check("bind_buffers_base", drain());

    glBindBuffersRange(GL_UNIFORM_BUFFER, 0, 1, one, (const GLintptr[]){0},
                       (const GLsizeiptr[]){16});
    check("bind_buffers_range", drain());

    glBindTextures(0, 1, one);
    check("bind_textures", drain());

    glBindSamplers(0, 1, one);
    check("bind_samplers", drain());

    glBindImageTextures(0, 1, one);
    check("bind_image_textures", drain());

    /* And a generated name must still work, so the sentinel cannot be
       standing in for every name. */
    GLuint real = 0; glGenBuffers(1, &real);
    glBindBuffer(GL_ARRAY_BUFFER, real);
    const GLenum clean = drain();
    ++cases; if (clean == GL_NO_ERROR) ++passed;
    fprintf(stderr, "INFO %-38s got=0x%04x want=0x0000\n",
            "generated_name_still_binds", (unsigned)clean);
    printf("CASE generated_name_still_binds %s\n",
           clean == GL_NO_ERROR ? "PASS" : "FAIL");

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
