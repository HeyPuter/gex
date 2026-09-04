/* ARB_transform_feedback_overflow_query: did the capture run out of room?
 *
 * GL stops recording transform feedback on a *whole* primitive when a bound
 * buffer would overflow -- a buffer with room for three vertices records one
 * triangle, not one and two thirds -- and this extension asks whether that
 * happened. It is how an application that sizes its feedback buffer by guess
 * finds out the guess was too small.
 *
 * This driver already worked the answer out: both capture passes compare what
 * a draw assembled against what the bound buffers had room for, because the
 * primitives-written counter has to report the second rather than the first.
 * The predicate is that comparison, which is why the extension cost a
 * comparison and not a mechanism.
 *
 *   `no_overflow`   a capture that fits reports false -- the case a driver
 *                   answering "true" always would fail
 *   `overflow`      a capture that does not fit reports true
 *   `counts_agree`  and in the overflowing case the primitives-written
 *                   counter reports the *smaller* number, so the predicate
 *                   and the counter tell the same story
 *
 * The third case is the one that catches a predicate bolted on beside a
 * counter that disagrees with it.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

#ifndef GL_TRANSFORM_FEEDBACK_OVERFLOW
#define GL_TRANSFORM_FEEDBACK_OVERFLOW 0x82EC
#endif

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const GLubyte *one = glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (one && strcmp((const char *)one, want) == 0) return 1;
    }
    return 0;
}

/* Capture `points` points into a buffer sized for `room` of them, and report
 * both the overflow predicate and the primitives-written count. */
static void capture(GLuint program, GLuint feedback, int points, int room,
                    GLuint *overflowed, GLuint *written) {
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, feedback);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER,
                 (GLsizeiptr)room * (GLsizeiptr)sizeof(float), NULL,
                 GL_DYNAMIC_READ);
    GLuint overflow_query = 0, written_query = 0;
    glGenQueries(1, &overflow_query);
    glGenQueries(1, &written_query);
    glUseProgram(program);
    glEnable(GL_RASTERIZER_DISCARD);
    glBeginQuery(GL_TRANSFORM_FEEDBACK_OVERFLOW, overflow_query);
    glBeginQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, written_query);
    glBeginTransformFeedback(GL_POINTS);
    glDrawArrays(GL_POINTS, 0, points);
    glEndTransformFeedback();
    glEndQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN);
    glEndQuery(GL_TRANSFORM_FEEDBACK_OVERFLOW);
    glDisable(GL_RASTERIZER_DISCARD);
    *overflowed = 0;
    *written = 0;
    glGetQueryObjectuiv(overflow_query, GL_QUERY_RESULT, overflowed);
    glGetQueryObjectuiv(written_query, GL_QUERY_RESULT, written);
    glDeleteQueries(1, &overflow_query);
    glDeleteQueries(1, &written_query);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have = have_extension("GL_ARB_transform_feedback_overflow_query");
    fprintf(stderr, "INFO version=%s overflow_query=%d\n",
            glGetString(GL_VERSION), have);
    if (!have) {
        fprintf(stderr, "INFO extension absent; nothing to test\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    GLuint vao = 0, feedback = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &feedback);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, feedback);

    static const char *vs_src =
        "#version 150 core\n"
        "out float value;\n"
        "void main() {\n"
        "  gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
        "  value = float(gl_VertexID);\n"
        "}\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    const char *varying = "value";
    glTransformFeedbackVaryings(program, 1, &varying, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint ok = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[600] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    /* Eight points into room for sixteen: everything fits. */
    GLuint overflowed = 0, written = 0;
    capture(program, feedback, 8, 16, &overflowed, &written);
    GLenum error = glGetError();
    fprintf(stderr, "INFO no_overflow overflowed=%u written=%u err=0x%04x\n",
            overflowed, written, (unsigned)error);
    result("no_overflow",
           error == GL_NO_ERROR && overflowed == 0 && written == 8);

    /* Sixteen points into room for four: twelve have nowhere to go. */
    capture(program, feedback, 16, 4, &overflowed, &written);
    error = glGetError();
    fprintf(stderr, "INFO overflow overflowed=%u written=%u err=0x%04x\n",
            overflowed, written, (unsigned)error);
    result("overflow", error == GL_NO_ERROR && overflowed != 0);

    /* And the counter agrees: it reports what fit, not what was asked for. */
    result("counts_agree", error == GL_NO_ERROR && written == 4);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
