/* Changing programs while transform feedback is PAUSED is legal.
 *
 * GL forbids glUseProgram while transform feedback is *active*, and explicitly
 * permits it once paused -- that is what pausing is for. The CTS checks it in
 * KHR-GL44.transform_feedback.api_errors_test, which passes natively and fails
 * in a browser with "glUseProgram unexpectedly failed when transform feedback
 * is paused", so something on the guest path reports an error the host does
 * not. This is that check on its own, small enough to iterate on.
 *
 * Every step clears the error queue first, so a stale error from an earlier
 * call cannot be mistaken for this one -- GL errors are sticky, and the whole
 * question here is *which* call produced one.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases = 0, failures = 0;
static void report(const char* name, int ok, const char* detail) {
    ++cases;
    if (!ok) ++failures;
    printf("CASE %s %s%s%s\n", name, ok ? "PASS" : "FAIL",
           detail && *detail ? " -- " : "", detail ? detail : "");
}
static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }
#define NEED(c, m) do { if (!(c)) { fprintf(stderr, "xfb-pause failed: %s\n", m); return __LINE__; } } while (0)

static const char* vs_src =
    "#version 400\n"
    "out float captured;\n"
    "void main() { captured = float(gl_VertexID); gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char* fs_src =
    "#version 400\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(1.0); }\n";

static GLuint build(int with_feedback) {
    GLuint v = glCreateShader(GL_VERTEX_SHADER), f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(v, 1, &vs_src, NULL); glCompileShader(v);
    glShaderSource(f, 1, &fs_src, NULL); glCompileShader(f);
    GLuint p = glCreateProgram();
    glAttachShader(p, v); glAttachShader(p, f);
    if (with_feedback) {
        static const char* varyings[] = {"captured"};
        glTransformFeedbackVaryings(p, 1, varyings, GL_INTERLEAVED_ATTRIBS);
    }
    glLinkProgram(p);
    glDeleteShader(v); glDeleteShader(f);
    return p;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    NEED(eglInitialize(display, NULL, NULL), "eglInitialize");
    NEED(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    const EGLint ca[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE };
    EGLConfig config = NULL; EGLint n = 0;
    NEED(eglChooseConfig(display, ca, &config, 1, &n) && n >= 1, "eglChooseConfig");
    const EGLint sa[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext ctx = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surf = eglCreatePbufferSurface(display, config, sa);
    NEED(ctx != EGL_NO_CONTEXT && surf != EGL_NO_SURFACE, "context/surface");
    NEED(eglMakeCurrent(display, surf, surf, ctx), "eglMakeCurrent");

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    const GLuint capture = build(1), plain = build(0);
    GLint linked = 0;
    glGetProgramiv(capture, GL_LINK_STATUS, &linked);
    NEED(linked, "link the capture program");

    GLuint xfb = 0, buffer = 0;
    glGenTransformFeedbacks(1, &xfb);
    glBindTransformFeedback(GL_TRANSFORM_FEEDBACK, xfb);
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, buffer);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 256, NULL, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, buffer);
    glUseProgram(capture);
    glEnable(GL_RASTERIZER_DISCARD);
    drain();

    glBeginTransformFeedback(GL_POINTS);
    {
        const GLenum err = glGetError();
        char d[48]; snprintf(d, sizeof d, "err=0x%x", (unsigned)err);
        report("begin_transform_feedback", err == GL_NO_ERROR, d);
    }

    /* Active, not paused: GL requires this to be rejected. */
    glUseProgram(plain);
    {
        const GLenum err = glGetError();
        char d[64]; snprintf(d, sizeof d, "err=0x%x (want 0x502)", (unsigned)err);
        report("use_program_while_active_is_refused",
               err == GL_INVALID_OPERATION, d);
    }

    glPauseTransformFeedback();
    {
        const GLenum err = glGetError();
        char d[48]; snprintf(d, sizeof d, "err=0x%x", (unsigned)err);
        report("pause_transform_feedback", err == GL_NO_ERROR, d);
    }

    /* Paused: now it must be allowed. This is the case that diverges. */
    glUseProgram(plain);
    {
        const GLenum err = glGetError();
        char d[64]; snprintf(d, sizeof d, "err=0x%x (want 0)", (unsigned)err);
        report("use_program_while_paused_is_allowed", err == GL_NO_ERROR, d);
    }

    glUseProgram(capture);
    drain();
    glResumeTransformFeedback();
    glEndTransformFeedback();
    glDisable(GL_RASTERIZER_DISCARD);
    {
        const GLenum err = glGetError();
        char d[48]; snprintf(d, sizeof d, "err=0x%x", (unsigned)err);
        report("resume_and_end", err == GL_NO_ERROR, d);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures, failures);
    return failures ? 1 : 0;
}
