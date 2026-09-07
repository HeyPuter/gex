/* Which primitive modes glDrawTransformFeedbackInstanced rejects, and how the
 * CTS decides which one to hand it.
 *
 * KHR-GL44.transform_feedback.api_errors_test does not hard-code an invalid
 * mode. It counts upward out of the set of modes it believes are supported:
 *
 *     GL_POINTS .. GL_QUADS, plus GL_PATCHES when tessellation is there,
 *     plus GL_QUAD_STRIP and GL_POLYGON *only in a compatibility context*
 *
 * so the first unsupported enum is 8 (GL_QUAD_STRIP) in core and 10
 * (GL_LINES_ADJACENCY) in compatibility -- and then requires GL_INVALID_ENUM.
 * The mode it picks therefore depends on the profile, and so does whether the
 * answer is right. This driver advertises quads, quad-strip and polygon, and
 * GL_LINES_ADJACENCY is a legal draw mode wherever geometry shaders are.
 *
 * So this reports rather than asserts most of it: the useful output is the
 * profile, the mode the CTS would compute from it, and the error each
 * candidate actually produces on each path.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

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
    fprintf(stderr, "INFO %-30s got=0x%04llx want=0x%04llx\n", name, got, want);
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

static const char *vs_src =
    "#version 440 core\n"
    "out float tf_out;\n"
    "void main() { tf_out = float(gl_VertexID);\n"
    "              gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char *fs_src =
    "#version 440 core\n"
    "out vec4 c; void main() { c = vec4(1.0); }\n";

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
    const int core = (profile & 0x1) != 0;
    const int tess = has_ext("GL_ARB_tessellation_shader");
    /* The CTS does not only look at the context type it asked for. It also
       ORs in `hasExtension(gl, core(3,1), "GL_ARB_compatibility")`, so the
       *extension list* can flip is_compatibility on its own -- and with it
       which enum the test treats as invalid. If this differs between the two
       paths, so does the mode, and the whole failure follows from it. */
    const int arb_compat = has_ext("GL_ARB_compatibility");
    GLint next_count = 0; glGetIntegerv(GL_NUM_EXTENSIONS, &next_count); drain();
    /* The CTS's own arithmetic, reproduced. */
    const int cts_mode = core ? 8 : 10;
    fprintf(stderr, "INFO version=%s profile_mask=0x%x tessellation=%d\n",
            (const char *)glGetString(GL_VERSION), (unsigned)profile, tess);
    fprintf(stderr, "INFO ARB_compatibility=%d extensions=%d\n",
            arb_compat, (int)next_count);
    fprintf(stderr, "INFO the CTS would pass mode=0x%04x on this context\n",
            (unsigned)cts_mode);

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
    eq("program_links", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n",
                          cases, passed, cases - passed); return 1; }

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    GLuint tf = 0; glGenTransformFeedbacks(1, &tf);
    glBindTransformFeedback(GL_TRANSFORM_FEEDBACK, tf);
    GLuint buf = 0; glGenBuffers(1, &buf);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, buf);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 1024, NULL, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, buf);
    glUseProgram(prog);
    /* Give the object a recorded primitive count, so a failure below is about
       the mode and not about an unused transform feedback object. */
    glBeginTransformFeedback(GL_POINTS);
    glDrawArrays(GL_POINTS, 0, 4);
    glEndTransformFeedback();
    drain();

    static const struct { const char *name; GLenum mode; } probes[] = {
        { "quad_strip_0x08",       GL_QUAD_STRIP },
        { "polygon_0x09",          GL_POLYGON },
        { "lines_adjacency_0x0a",  0x000A },
        { "nonsense_0xffff",       0xFFFF },
    };
    for (unsigned i = 0; i < sizeof(probes)/sizeof(probes[0]); ++i) {
        glDrawTransformFeedbackInstanced(probes[i].mode, tf, 1);
        const GLenum e = sole();
        fprintf(stderr, "INFO %-30s err=0x%04x%s\n", probes[i].name,
                (unsigned)e,
                (unsigned)probes[i].mode == (unsigned)cts_mode
                    ? "   <-- the mode the CTS uses here" : "");
    }

    /* The one thing that is true in every profile: a mode that is not a
       primitive type at all must be rejected. If this fails, mode validation
       is not reaching the driver, and the CTS's own choice is a side issue. */
    glDrawTransformFeedbackInstanced(0xFFFF, tf, 1);
    eq("nonsense_mode_is_invalid_enum", sole(), GL_INVALID_ENUM);

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
