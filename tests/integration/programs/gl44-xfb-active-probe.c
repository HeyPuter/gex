/* The errors GL owes while transform feedback is active.
 *
 * KHR-GL44.transform_feedback.api_errors_test fails in the browser and passes
 * natively, and reports only "Fail" across more than twenty checks. Three of
 * them are state-dependent in a way nothing else here tests: while a feedback
 * capture is *active*, rebinding its buffer or changing the program is
 * GL_INVALID_OPERATION.
 *
 * The interesting possibility this separates: all three depend on the host's
 * GL agreeing that feedback is active. If glBeginTransformFeedback quietly
 * failed there, none of the three would raise and every one would look like a
 * missing error rather than the one cause. So the first question is whether
 * begin succeeded at all.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

static void eq(const char *name, GLenum got, GLenum want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-38s got=0x%04x want=0x%04x\n", name,
            (unsigned)got, (unsigned)want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 430 core\n"
    "out float captured;\n"
    "void main() { captured = 2.0; gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";

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

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    const char *captured[] = { "captured" };
    glTransformFeedbackVaryings(program, 1, captured, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[512] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }

    GLuint buf = 0; glGenBuffers(1, &buf);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, buf);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 256, NULL, GL_DYNAMIC_COPY);
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glUseProgram(program);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, buf);
    eq("setup_is_clean", drain(), GL_NO_ERROR);

    /* The question everything else depends on. */
    glBeginTransformFeedback(GL_POINTS);
    eq("begin_is_clean", drain(), GL_NO_ERROR);

    glBindBufferRange(GL_TRANSFORM_FEEDBACK_BUFFER, 0, buf, 0, 16);
    eq("bind_range_while_active", drain(), GL_INVALID_OPERATION);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, buf);
    eq("bind_base_while_active", drain(), GL_INVALID_OPERATION);
    glUseProgram(0);
    eq("use_program_while_active", drain(), GL_INVALID_OPERATION);
    glBeginTransformFeedback(GL_POINTS);
    eq("begin_while_active", drain(), GL_INVALID_OPERATION);

    glEndTransformFeedback();
    eq("end_is_clean", drain(), GL_NO_ERROR);
    glEndTransformFeedback();
    eq("end_while_inactive", drain(), GL_INVALID_OPERATION);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
