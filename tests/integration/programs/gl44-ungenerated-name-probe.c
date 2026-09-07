/* Whether binding a name the application never generated raises the error GL
 * owes, and whether an unknown transform feedback name is rejected.
 *
 * The host transport resolves guest object names through its own maps. Its
 * `lookup` returns **zero** for a name it does not know, and zero is a legal
 * argument to every one of these calls -- an unbind, or the default
 * framebuffer, or the default vertex array. So an unknown name did not merely
 * lose its error, it performed a *different legal operation*: a stale
 * framebuffer name silently redirected rendering to the window.
 *
 * `lookup_bindable` exists for exactly this and hands GL a name it has never
 * generated, so GL raises the error itself instead of the transport guessing.
 * It had three callers out of the eight sites that wanted it.
 *
 * The last case is the one that found this. KHR-GL44.transform_feedback.
 * api_errors_test ends by searching for a name glIsTransformFeedback rejects
 * and requiring GL_INVALID_VALUE from glDrawTransformFeedback on it -- but
 * `lookup` turned that name into zero, the *default* transform feedback
 * object, and drawing from it is perfectly legal. Read that case's failure
 * message with suspicion: the CTS prints "glUseProgram unexpectedly failed
 * when transform feedback is paused" there, a copy-paste of the check above
 * it, and the message names a call the test is not making.
 *
 * All of this is invisible natively: the reference is a compatibility context
 * and the driver is asked with names it generated itself.
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
    fprintf(stderr, "INFO %-34s got=0x%04llx want=0x%04llx\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }

/* One error, reported as the first in the queue -- a call that raises two
   would otherwise let the next check trip over the remainder. */
static GLenum sole_error(void) {
    GLenum first = glGetError();
    drain();
    return first;
}

/* A name of this kind that GL itself says is not one. Asking rather than
   assuming: the transport allocates guest names, so a literal like 1 may well
   be live, and a probe that tested a *live* name would pass while proving
   nothing. */
static GLuint unused_name(GLboolean (*is)(GLuint)) {
    GLuint n = 1;
    while (is(n)) {
        if (++n == 0x10000u) return 0; /* give up; caller skips */
    }
    return n;
}

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

    GLint profile = 0;
    glGetIntegerv(GL_CONTEXT_PROFILE_MASK, &profile);
    drain();
    fprintf(stderr, "INFO version=%s profile_mask=0x%x\n",
            (const char *)glGetString(GL_VERSION), (unsigned)profile);

    /* Which errors are owed depends on the profile, and that split is the
       whole point of this probe.

       Samplers and transform feedback objects **never** had implicit
       creation, in either profile: an ungenerated name is an error, always.
       Those two are asserted.

       Framebuffers, renderbuffers and vertex arrays are **profile-dependent**.
       A core context owes GL_INVALID_OPERATION; a compatibility context has
       EXT_framebuffer_object semantics and must implicitly *create* the
       object, which is what llvmpipe 26.1 does -- it binds the arbitrary name
       and raises nothing. The guest chooses its profile per EGL context, so
       both really occur: the CTS asks for core, a default eglCreateContext
       like this one gets compatibility. The host transport is not told which,
       so it cannot yet answer either, and all three currently resolve the
       unknown name to zero -- which in compatibility means the *default
       framebuffer*, so a stale name silently redirects drawing to the window.

       Those three are therefore **reported and not asserted**. Asserting them
       would need the profile on the wire first, and a probe that fails for a
       known missing feature stops being a regression gate. The numbers are
       printed so the gap stays visible in the log rather than only in a
       document. */
    const int core = (profile & 0x1 /* GL_CONTEXT_CORE_PROFILE_BIT */) != 0;
    GLuint name;

    name = unused_name(glIsFramebuffer);
    if (name) {
        glBindFramebuffer(GL_FRAMEBUFFER, name);
        const GLenum err = sole_error();
        GLint bound = -1;
        glGetIntegerv(GL_FRAMEBUFFER_BINDING, &bound);
        drain();
        /* bound == name is compatibility's implicit creation; bound == 0 with
           no error is the transport resolving the unknown name to the default
           framebuffer, which is the defect. */
        fprintf(stderr, "INFO %-34s err=0x%04x binding=%d (want %s)\n",
                "gap:bind_framebuffer", (unsigned)err, bound,
                core ? "err=0x0502 binding=0" : "err=0 binding=name");
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        drain();
    }

    name = unused_name(glIsRenderbuffer);
    if (name) {
        glBindRenderbuffer(GL_RENDERBUFFER, name);
        const GLenum err = sole_error();
        fprintf(stderr, "INFO %-34s err=0x%04x (want %s)\n",
                "gap:bind_renderbuffer", (unsigned)err,
                core ? "0x0502" : "0");
        glBindRenderbuffer(GL_RENDERBUFFER, 0);
        drain();
    }

    name = unused_name(glIsVertexArray);
    if (name) {
        GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
        drain();
        glBindVertexArray(name);
        const GLenum err = sole_error();
        GLint bound = -1;
        glGetIntegerv(GL_VERTEX_ARRAY_BINDING, &bound);
        drain();
        fprintf(stderr, "INFO %-34s err=0x%04x binding_is_live=%d (want %s)\n",
                "gap:bind_vertex_array", (unsigned)err, bound == (GLint)vao,
                core ? "0x0502 live" : "0 name");
        glBindVertexArray(vao);
        drain();
    }

    /* Asserted: no implicit creation in any profile. */
    name = unused_name(glIsSampler);
    if (name) {
        glBindSampler(0, name);
        eq("bind_sampler_ungenerated", sole_error(), GL_INVALID_OPERATION);
    }

    /* The CTS's own check, reproduced. GL_INVALID_VALUE, not
       INVALID_OPERATION: the id is not a transform feedback object at all.
       This is the one KHR-GL44.transform_feedback.api_errors_test ends on. */
    name = unused_name(glIsTransformFeedback);
    if (name) {
        glDrawTransformFeedback(GL_POINTS, name);
        eq("draw_transform_feedback_unknown", sole_error(), GL_INVALID_VALUE);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
