/* ARB_transform_feedback2: pause, resume, and glDrawTransformFeedback.
 *
 * The extension is really one piece of state -- how far into each bound buffer
 * the last capture got -- seen from three directions. Pausing unbinds the
 * targets and resuming binds them back asking to continue, an explicit offset
 * says where to start, and glDrawTransformFeedback asks how far it got in
 * order to draw that many vertices back.
 *
 * So the probe captures three values, pauses and draws three more that must
 * not be recorded, resumes and captures three more. A driver that ignored the
 * resume offset would overwrite from the start and leave 7,8,9; one that
 * ignored the pause would leave 1,2,3,4,5,6; one that got it right leaves
 * 1,2,3,7,8,9. The three outcomes are distinguishable, which is the point.
 *
 * The same buffer is then drawn back with glDrawTransformFeedback into a row
 * of pixels, each captured value lighting the pixel at its own index. That
 * checks the recorded count as well as the contents: a draw that took the
 * whole buffer rather than the recorded part would light pixel 0 as well.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* Captures its input untouched. The position is off-screen for the capture
 * draws, which record vertices rather than produce pixels. */
static const char *capture_vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in float value;\n"
    "out float captured;\n"
    "void main() {\n"
    "    captured = value;\n"
    "    gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
    "}\n";

/* Draws a captured value as the pixel at that index of a 16-wide row. */
static const char *replay_vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in float value;\n"
    "void main() {\n"
    "    float x = (value + 0.5) / 16.0 * 2.0 - 1.0;\n"
    "    gl_Position = vec4(x, 0.0, 0.0, 1.0);\n"
    "}\n";

static const char *white_fragment_source =
    "#version 330 core\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = vec4(1.0, 1.0, 1.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

static GLuint link(GLuint vertex, GLuint fragment, const char *capture) {
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    if (capture)
        glTransformFeedbackVaryings(program, 1, &capture,
                                    GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "link failed: %s\n", log);
        return 0;
    }
    return program;
}

#define ROW 16

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    int advertised = 0;
    GLint extension_count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &extension_count);
    for (GLint i = 0; i < extension_count && !advertised; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, "GL_ARB_transform_feedback2") == 0)
            advertised = 1;
    }
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO advertised=%d\n", advertised);
    result("xfb2_advertised", advertised);
    if (!advertised) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    GLuint capture_vs = compile(GL_VERTEX_SHADER, capture_vertex_source);
    GLuint replay_vs = compile(GL_VERTEX_SHADER, replay_vertex_source);
    GLuint white_fs = compile(GL_FRAGMENT_SHADER, white_fragment_source);
    if (!capture_vs || !replay_vs || !white_fs) return 2;
    GLuint capture_program = link(capture_vs, white_fs, "captured");
    GLuint replay_program = link(replay_vs, white_fs, NULL);
    if (!capture_program || !replay_program) {
        result("xfb2_programs_link", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    /* Nine source values in three groups of three: recorded, skipped while
     * paused, then recorded again after resuming. */
    static const float source[9] = {1, 2, 3, 4, 5, 6, 7, 8, 9};
    GLuint source_buffer = 0;
    glGenBuffers(1, &source_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, source_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(source), source, GL_STATIC_DRAW);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 1, GL_FLOAT, GL_FALSE, sizeof(float), NULL);

    /* Room for more than is recorded, so a capture that ran past its recorded
     * length would leave a visible tail. */
    float initial[ROW];
    for (int i = 0; i < ROW; ++i) initial[i] = -1.0f;
    GLuint capture_buffer = 0;
    glGenBuffers(1, &capture_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, capture_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(initial), initial, GL_DYNAMIC_COPY);

    GLuint feedback = 0;
    glGenTransformFeedbacks(1, &feedback);
    glBindTransformFeedback(GL_TRANSFORM_FEEDBACK, feedback);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, capture_buffer);

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, ROW, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, ROW, 1);

    glBindBuffer(GL_ARRAY_BUFFER, source_buffer);
    glVertexAttribPointer(0, 1, GL_FLOAT, GL_FALSE, sizeof(float), NULL);
    glUseProgram(capture_program);
    glEnable(GL_RASTERIZER_DISCARD);
    glBeginTransformFeedback(GL_POINTS);
    glDrawArrays(GL_POINTS, 0, 3);          /* records 1,2,3 */
    glPauseTransformFeedback();
    glDrawArrays(GL_POINTS, 3, 3);          /* 4,5,6 must not be recorded */
    glResumeTransformFeedback();
    glDrawArrays(GL_POINTS, 6, 3);          /* records 7,8,9 after them */
    glEndTransformFeedback();
    glDisable(GL_RASTERIZER_DISCARD);

    static const float want[6] = {1, 2, 3, 7, 8, 9};
    float got[ROW];
    for (int i = 0; i < ROW; ++i) got[i] = -99.0f;
    glBindBuffer(GL_ARRAY_BUFFER, capture_buffer);
    glGetBufferSubData(GL_ARRAY_BUFFER, 0, sizeof(got), got);
    GLenum error = glGetError();
    int ok = error == GL_NO_ERROR;
    for (int i = 0; i < 6; ++i)
        if (got[i] != want[i]) ok = 0;
    /* Nothing beyond the six recorded values may have been touched. */
    if (got[6] != -1.0f) ok = 0;
    fprintf(stderr,
            "INFO pause_resume got=%.0f,%.0f,%.0f,%.0f,%.0f,%.0f next=%.0f "
            "want=1,2,3,7,8,9 next=-1 err=0x%04x\n",
            got[0], got[1], got[2], got[3], got[4], got[5], got[6],
            (unsigned)error);
    result("xfb2_pause_resume", ok);

    /* Draw the recording back. Each value lights the pixel at its own index,
     * so the lit set says both what was captured and how much of it the draw
     * believed was there. */
    glBindBuffer(GL_ARRAY_BUFFER, capture_buffer);
    glVertexAttribPointer(0, 1, GL_FLOAT, GL_FALSE, sizeof(float), NULL);
    glUseProgram(replay_program);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawTransformFeedback(GL_POINTS, feedback);
    unsigned char pixels[ROW * 4];
    memset(pixels, 0xAB, sizeof(pixels));
    glReadPixels(0, 0, ROW, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    error = glGetError();
    char lit[ROW + 1];
    for (int i = 0; i < ROW; ++i) lit[i] = pixels[i * 4] > 127 ? '#' : '.';
    lit[ROW] = 0;
    /* Pixels 1,2,3,7,8,9 and nothing else. */
    const char *want_lit = ".###...###......";
    ok = error == GL_NO_ERROR && strcmp(lit, want_lit) == 0;
    fprintf(stderr, "INFO draw_feedback lit=%s want=%s err=0x%04x\n", lit,
            want_lit, (unsigned)error);
    result("xfb2_draw_feedback", ok);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
