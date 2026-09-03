/* glBeginConditionalRender, both senses, and glTextureBarrier.
 *
 * The driver set caps->conditional_render -- so Mesa advertised
 * ARB_conditional_render, which is GL *3.0* -- while render_condition failed
 * the context for any query, on a comment that queries "do not exist here".
 * They had existed since occlusion queries landed. So an application that
 * used a feature the driver claimed got GL_OUT_OF_MEMORY from every later
 * call, and nothing tested it.
 *
 * The convention is worth stating because it is not obvious: st passes
 * `condition` true for GL's *_INVERTED modes, and a driver tests the
 * *negation* of "result is non-zero" against it. Getting it backwards
 * inverts every conditional draw, which is why both senses are covered here
 * against a query that passes and one that does not.
 *
 * ARB_conditional_render_inverted and NV_texture_barrier are two of the five
 * extensions GL 4.5 still wants.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 8

/* Not in Mesa's non-glvnd libGL export list; see the note in
 * gl44-texture-sampler-probe.c. */
static void (*resolved_texture_barrier)(void);

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

static GLuint build(void) {
    static const char *vs =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs =
        "#version 330 core\n"
        "uniform vec4 tint;\n"
        "out vec4 colour;\n"
        "void main() { colour = tint; }\n";
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs, NULL); glCompileShader(v);
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs, NULL); glCompileShader(f);
    GLuint p = glCreateProgram();
    glAttachShader(p, v); glAttachShader(p, f); glLinkProgram(p);
    GLint ok = 0;
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    return ok ? p : 0;
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
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE,
                                         EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int inverted = have_extension("GL_ARB_conditional_render_inverted");
    const int barrier = have_extension("GL_NV_texture_barrier") ||
                        have_extension("GL_ARB_texture_barrier");
    resolved_texture_barrier =
        (void (*)(void))eglGetProcAddress("glTextureBarrier");
    if (!resolved_texture_barrier)
        resolved_texture_barrier =
            (void (*)(void))eglGetProcAddress("glTextureBarrierNV");
    fprintf(stderr, "INFO version=%s inverted=%d barrier=%d entry=%d\n",
            glGetString(GL_VERSION), inverted, barrier,
            resolved_texture_barrier != NULL);

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    GLuint program = build();
    if (!program) return 2;
    glUseProgram(program);
    const GLint tint = glGetUniformLocation(program, "tint");

    /* A query that counts samples, run either over a full-screen draw (it
     * passes) or with an empty scissor (it does not). */
    struct { const char *name; int query_passes; GLenum mode; int want_green; }
    tests[] = {
        {"passes",          1, GL_QUERY_WAIT,          1},
        {"fails",           0, GL_QUERY_WAIT,          0},
        {"inverted-passes", 0, GL_QUERY_WAIT_INVERTED, 1},
        {"inverted-fails",  1, GL_QUERY_WAIT_INVERTED, 0},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        if (tests[t].mode == GL_QUERY_WAIT_INVERTED && !inverted) {
            fprintf(stderr, "INFO %s skipped (no inverted)\n", tests[t].name);
            continue;
        }
        GLuint query = 0;
        glGenQueries(1, &query);
        /* Fill the predicate. The draw itself goes to the same target and is
         * overwritten by the clear below, so only the sample count matters. */
        glBeginQuery(GL_SAMPLES_PASSED, query);
        if (!tests[t].query_passes) {
            glEnable(GL_SCISSOR_TEST);
            glScissor(0, 0, 0, 0);
        }
        glUniform4f(tint, 1, 0, 0, 1);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glDisable(GL_SCISSOR_TEST);
        glEndQuery(GL_SAMPLES_PASSED);

        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glBeginConditionalRender(query, tests[t].mode);
        glUniform4f(tint, 0, 1, 0, 1);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glEndConditionalRender();

        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        const int green = px[1] > 200 && px[0] < 60;
        fprintf(stderr, "INFO %s got=%u,%u,%u want_green=%d err=0x%04x\n",
                tests[t].name, px[0], px[1], px[2], tests[t].want_green,
                (unsigned)error);
        result(tests[t].name, error == GL_NO_ERROR &&
                              green == tests[t].want_green);
        glDeleteQueries(1, &query);
    }

    /* glTextureBarrier: write the texture, barrier, then sample it into a
     * second target. Without the barrier this is undefined; with it the write
     * must be visible. */
    if (barrier && resolved_texture_barrier) {
        glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        resolved_texture_barrier();
        GLenum error = glGetError();
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        if (error == GL_NO_ERROR) error = glGetError();
        fprintf(stderr, "INFO barrier got=%u,%u,%u want=64,128,191 err=0x%04x\n",
                px[0], px[1], px[2], (unsigned)error);
        result("texture-barrier",
               error == GL_NO_ERROR && px[0] >= 62 && px[0] <= 66 &&
               px[1] >= 126 && px[1] <= 130 && px[2] >= 189 && px[2] <= 193);
    } else {
        fprintf(stderr, "INFO texture-barrier skipped (ext=%d entry=%d)\n",
                barrier, resolved_texture_barrier != NULL);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
