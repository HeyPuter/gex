/* What a *core* profile context accepts, where a compatibility one is right to
 * accept more.
 *
 * Every other probe here takes whatever eglCreateContext gives by default,
 * which is compatibility -- so a rule that only core enforces has never been
 * measured. Two of them matter:
 *
 *   - GL_QUAD_STRIP and GL_POLYGON are primitive modes in compatibility and
 *     *removed* in core, where passing them is GL_INVALID_ENUM. This driver
 *     advertises quads, quad-strip and polygon to serve unfilled quads, and
 *     that support must not leak into a core context.
 *
 *   - GL_ARB_compatibility must not be advertised by a core context. The CTS
 *     reads it directly -- KHR-GL44.transform_feedback.api_errors_test ORs it
 *     into its own is_compatibility, and uses that to decide which enum is
 *     invalid: 8 (GL_QUAD_STRIP) when it believes the context is core, 10
 *     (GL_LINES_ADJACENCY) when it believes it is compatibility. The second is
 *     a legal draw mode wherever geometry shaders exist, so a driver that
 *     claims compatibility in a core context sends the test to ask for an
 *     error that is not owed, and the test fails on a correct answer.
 *
 * So the two are the same defect seen twice, and this probe is the measurement
 * for both.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#ifndef EGL_CONTEXT_MAJOR_VERSION
#define EGL_CONTEXT_MAJOR_VERSION 0x3098
#endif
#ifndef EGL_CONTEXT_MINOR_VERSION
#define EGL_CONTEXT_MINOR_VERSION 0x30FB
#endif
#ifndef EGL_CONTEXT_OPENGL_PROFILE_MASK
#define EGL_CONTEXT_OPENGL_PROFILE_MASK 0x30FD
#endif
#ifndef EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT
#define EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT 0x00000001
#endif
#ifndef GL_QUAD_STRIP
#define GL_QUAD_STRIP 0x0008
#endif
#ifndef GL_POLYGON
#define GL_POLYGON 0x0009
#endif

static int cases, passed;
static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-32s got=0x%04llx want=0x%04llx\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }
static GLenum sole(void) { GLenum e = glGetError(); drain(); return e; }

static int has_ext(const char *want) {
    GLint n = 0; glGetIntegerv(GL_NUM_EXTENSIONS, &n); drain();
    for (GLint i = 0; i < n; ++i) {
        const char *e = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (e && strcmp(e, want) == 0) { drain(); return 1; }
    }
    drain();
    return 0;
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

    /* A 3.2 core context is the lowest that has a core profile at all, and
       asking for exactly it keeps the request satisfiable everywhere. */
    const EGLint core_attrs[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3,
        EGL_CONTEXT_MINOR_VERSION, 2,
        EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
        EGL_NONE
    };
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, core_attrs);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) {
        fprintf(stderr, "INFO no core-profile context available (egl 0x%04x)\n",
                (unsigned)eglGetError());
        /* Reported, not asserted: a path that cannot make a core context has
           nothing to say about core rules, and must not read as a failure. */
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    GLint profile = 0;
    glGetIntegerv(GL_CONTEXT_PROFILE_MASK, &profile);
    drain();
    fprintf(stderr, "INFO version=%s profile_mask=0x%x\n",
            (const char *)glGetString(GL_VERSION), (unsigned)profile);
    eq("context_is_core", (profile & 0x1) != 0, 1);

    /* The extension the CTS reads to decide what "invalid" means. */
    eq("no_arb_compatibility_in_core", has_ext("GL_ARB_compatibility"), 0);

    /* glGetString(GL_EXTENSIONS) is *removed* in core -- the monolithic string
       is compatibility-only and asking for it must be GL_INVALID_ENUM. It
       matters beyond tidiness: glu::hasExtension picks between this and
       glGetStringi by version, and a driver that answers the legacy query in
       a core context can tell a test it is a compatibility context. */
    const GLubyte *legacy = glGetString(GL_EXTENSIONS);
    eq("legacy_extension_string_refused", sole(), GL_INVALID_ENUM);
    eq("legacy_extension_string_is_null", legacy == NULL, 1);

    /* Removed in core: passing either is GL_INVALID_ENUM. glDrawArrays is the
       plainest path to the same validation the transform feedback draws use. */
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    drain();
    glDrawArrays(GL_QUAD_STRIP, 0, 4);
    eq("quad_strip_rejected_in_core", sole(), GL_INVALID_ENUM);
    glDrawArrays(GL_POLYGON, 0, 4);
    eq("polygon_rejected_in_core", sole(), GL_INVALID_ENUM);
    glDrawArrays(GL_QUADS, 0, 4);
    eq("quads_rejected_in_core", sole(), GL_INVALID_ENUM);

    /* And now the same rejection through the transform feedback draw entry
       points, which is the combination the CTS actually exercises and the one
       glDrawArrays above does not prove. Each of these has its own validation
       path in Mesa, and in the browser each is a separate marshalled command,
       so an error raised by one is no evidence about another. */
    static const char *vs_src =
        "#version 150 core\n"
        "out float tf_out;\n"
        "void main() { tf_out = float(gl_VertexID);\n"
        "              gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
    static const char *fs_src =
        "#version 150 core\n"
        "out vec4 c; void main() { c = vec4(1.0); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs); glAttachShader(prog, fs);
    static const char *varying = "tf_out";
    glTransformFeedbackVaryings(prog, 1, &varying, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    eq("tf_program_links", linked, 1);
    if (linked) {
        GLuint tf = 0; glGenTransformFeedbacks(1, &tf);
        glBindTransformFeedback(GL_TRANSFORM_FEEDBACK, tf);
        GLuint buf = 0; glGenBuffers(1, &buf);
        glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, buf);
        glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 1024, NULL, GL_DYNAMIC_COPY);
        glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, buf);
        glUseProgram(prog);
        /* Recorded primitives, so a failure below is about the mode. */
        glBeginTransformFeedback(GL_POINTS);
        glDrawArrays(GL_POINTS, 0, 4);
        glEndTransformFeedback();
        drain();

        glDrawTransformFeedback(GL_QUAD_STRIP, tf);
        eq("tf_draw_quad_strip_in_core", sole(), GL_INVALID_ENUM);
        glDrawTransformFeedbackInstanced(GL_QUAD_STRIP, tf, 1);
        eq("tf_draw_instanced_quad_strip", sole(), GL_INVALID_ENUM);
        glDrawTransformFeedbackStreamInstanced(GL_QUAD_STRIP, tf, 0, 1);
        eq("tf_draw_stream_inst_quad_strip", sole(), GL_INVALID_ENUM);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
