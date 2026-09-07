/* How many errors glLinkProgram raises when its program is used by a paused
 * transform feedback object, and whether glUseProgram is clean afterwards.
 *
 * KHR-GL44.transform_feedback.api_errors_test fails in the browser and passes
 * natively, on this pair of checks and no other:
 *
 *     gl.linkProgram(program);
 *     if (GL_INVALID_OPERATION != gl.getError()) ...   // this one passes
 *     gl.useProgram(0);
 *     if (GL_NO_ERROR != gl.getError()) ...            // this one fails
 *
 * glGetError pops *one* error. So a link that raises two would let the first
 * check succeed on the expected INVALID_OPERATION and leave the second to trip
 * over the remainder -- which reads as "glUseProgram failed" while glUseProgram
 * did nothing wrong. Counting the queue is the only way to tell that apart from
 * an error glUseProgram really did raise, so this drains and counts rather than
 * asking once.
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
    fprintf(stderr, "INFO %-38s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* Drain, reporting how many there were and what the first one was. */
static unsigned drain_count(GLenum *first) {
    unsigned n = 0; GLenum e;
    *first = GL_NO_ERROR;
    while ((e = glGetError()) != GL_NO_ERROR) {
        if (n == 0) *first = e;
        ++n;
        if (n > 16) break;
    }
    return n;
}

static const char *vs_src =
    "#version 440 core\n"
    "out float tf_out;\n"
    "void main() { tf_out = float(gl_VertexID);\n"
    "              gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char *fs_src =
    "#version 440 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(1.0); }\n";

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
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    static const char *varying = "tf_out";
    glTransformFeedbackVaryings(program, 1, &varying, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    eq("program_links", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n", cases,
                          passed, cases - passed); return 1; }

    GLuint tf = 0; glGenTransformFeedbacks(1, &tf);
    glBindTransformFeedback(GL_TRANSFORM_FEEDBACK, tf);
    GLuint buffer = 0; glGenBuffers(1, &buffer);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, buffer);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 1024, NULL, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, buffer);
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glUseProgram(program);

    /* The CTS's own order, because the error accounting depends on it: each
       check consumes exactly one error, so a call that raises one too many or
       one too few shifts every check after it and the failure surfaces
       somewhere innocent. */
    GLenum first = GL_NO_ERROR;
    glBeginTransformFeedback(GL_POINTS);
    eq("begin_leaves_no_error", drain_count(&first), 0);

    /* Resuming something that is not paused is an error, exactly once. */
    glResumeTransformFeedback();
    const unsigned after_resume = drain_count(&first);
    fprintf(stderr, "INFO after stray resume: %u error(s), first=0x%04x\n",
            after_resume, (unsigned)first);
    eq("stray_resume_is_invalid_operation", first, GL_INVALID_OPERATION);
    eq("stray_resume_raises_exactly_one", after_resume, 1);

    glPauseTransformFeedback();
    eq("pause_leaves_no_error", drain_count(&first), 0);

    /* Feedback began as POINTS and this draws LINES. While *paused* that is
       explicitly legal -- the primitive mode only has to match while feedback
       is capturing -- so it must raise nothing at all. */
    glDrawArrays(GL_LINES, 0, 2);
    const unsigned after_mismatch = drain_count(&first);
    fprintf(stderr, "INFO after mismatched paused draw: %u error(s), "
            "first=0x%04x\n", after_mismatch, (unsigned)first);
    eq("paused_mismatched_draw_is_clean", after_mismatch, 0);

    /* Paused, and the program is the one the object is using. */
    GLint paused = 0, active = 0;
    glGetIntegerv(GL_TRANSFORM_FEEDBACK_BUFFER_PAUSED, &paused);
    glGetIntegerv(GL_TRANSFORM_FEEDBACK_BUFFER_ACTIVE, &active);
    drain_count(&first);
    eq("feedback_reports_paused", paused, 1);
    eq("feedback_reports_active", active, 1);

    /* The link is required to fail, and required to fail *once*. */
    glLinkProgram(program);
    const unsigned after_link = drain_count(&first);
    fprintf(stderr, "INFO after link: %u error(s), first=0x%04x\n", after_link,
            (unsigned)first);
    eq("link_raises_invalid_operation", first, GL_INVALID_OPERATION);
    eq("link_raises_exactly_one_error", after_link, 1);

    /* And unbinding the program while paused is legal. */
    glUseProgram(0);
    const unsigned after_use = drain_count(&first);
    fprintf(stderr, "INFO after useProgram(0): %u error(s), first=0x%04x\n",
            after_use, (unsigned)first);
    eq("use_program_zero_is_clean", after_use, 0);

    glUseProgram(program);
    drain_count(&first);
    glResumeTransformFeedback();
    glEndTransformFeedback();
    eq("teardown_is_clean", drain_count(&first), 0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
