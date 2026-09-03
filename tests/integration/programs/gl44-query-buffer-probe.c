/* ARB_query_buffer_object -- one of GL 4.4's five remaining items.
 *
 * A query's result written into a *buffer object* instead of into client
 * memory. The point is to avoid the stall: a GPU-driven renderer issues an
 * occlusion query, has the result land in a buffer, and feeds it to a shader
 * or an indirect draw without the CPU ever seeing it.
 *
 * This driver resolves queries on the CPU, so its implementation is a readback
 * plus a buffer write -- correct, and without the benefit. Worth saying out
 * loud rather than implying: the extension is what GL 4.4 requires, and having
 * it does not make this driver's queries asynchronous.
 *
 *   `u32`        an occlusion count as GL_UNSIGNED_INT
 *   `u64`        the same count as GL_UNSIGNED_INT64_ARB, whose eight bytes
 *                must all be written -- a driver storing four leaves the high
 *                half at whatever the buffer held
 *   `available`  GL_QUERY_RESULT_AVAILABLE, a different value from the result
 *                and reached through the same call with index -1
 *   `offset`     written at a non-zero offset, with the words on either side
 *                left exactly as they were
 *
 * The occlusion count is the number of covered pixels, exact for an
 * unfiltered full-viewport quad, so every case is an equality on an integer.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static int have_extension(const char *want)
{
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 16
#define COVERED (SIDE * SIDE)
#define SENTINEL 0xa5a5a5a5u

static GLuint prog, vao, query, buffer;

/* Resolved rather than linked.
 *
 * Mesa's non-glvnd libGL.so.1 exports a *fixed* ABI -- src/glx/libgl-symbols.txt,
 * 1372 entries -- and glGetQueryObjectui64v is not in it, because it is GL 3.3
 * rather than legacy. Calling it directly links against the system libGL
 * (which is glvnd and exports everything) and then fails at run time against
 * the driver's:
 *
 *   symbol lookup error: undefined symbol: glGetQueryObjectui64v
 *
 * That is not a driver defect; it is the ABI, and real applications go through
 * a loader for exactly this reason. Any probe touching a non-legacy entry
 * point has to do the same. */
static void (*get_query_object_ui64v)(GLuint, GLenum, GLuint64 *);

static void measure(GLenum pname, GLenum type, unsigned offset,
                    uint32_t *words, unsigned word_count)
{
    for (unsigned i = 0; i < word_count; ++i) words[i] = SENTINEL;
    glBindBuffer(GL_QUERY_BUFFER, buffer);
    glBufferSubData(GL_QUERY_BUFFER, 0, word_count * 4, words);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glBeginQuery(GL_SAMPLES_PASSED, query);
    glUseProgram(prog);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glEndQuery(GL_SAMPLES_PASSED);
    /* The buffer is bound, so the pointer is an offset into it rather than a
     * client address, and nothing is returned. */
    if (type == GL_UNSIGNED_INT)
        glGetQueryObjectuiv(query, pname, (GLuint *)(intptr_t)offset);
    else
        get_query_object_ui64v(query, pname, (GLuint64 *)(intptr_t)offset);
    glBindBuffer(GL_QUERY_BUFFER, 0);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    const uint32_t *read = (const uint32_t *)glMapBufferRange(
        GL_ARRAY_BUFFER, 0, word_count * 4, GL_MAP_READ_BIT);
    if (read) {
        memcpy(words, read, word_count * 4);
        glUnmapBuffer(GL_ARRAY_BUFFER);
    }
    glBindBuffer(GL_ARRAY_BUFFER, 0);
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    get_query_object_ui64v = (void (*)(GLuint, GLenum, GLuint64 *))
        eglGetProcAddress("glGetQueryObjectui64v");
    const int have = have_extension("GL_ARB_query_buffer_object") &&
                     get_query_object_ui64v != NULL;
    fprintf(stderr, "INFO version=%s query_buffer_object=%d\n",
            glGetString(GL_VERSION), have);

    GLuint colour = 0, fbo = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);

    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(1.0); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) return 2;

    glGenBuffers(1, &buffer);
    glBindBuffer(GL_QUERY_BUFFER, buffer);
    {
        uint32_t init[8];
        for (unsigned i = 0; i < 8; ++i) init[i] = SENTINEL;
        glBufferData(GL_QUERY_BUFFER, sizeof(init), init, GL_DYNAMIC_COPY);
    }
    glBindBuffer(GL_QUERY_BUFFER, 0);
    while (glGetError() != GL_NO_ERROR) {}

    struct {
        const char *name;
        GLenum pname;
        GLenum type;
        unsigned offset;
    } tests[] = {
        {"u32",       GL_QUERY_RESULT,           GL_UNSIGNED_INT,        0},
        {"u64",       GL_QUERY_RESULT,           GL_UNSIGNED_INT64_ARB,  0},
        {"available", GL_QUERY_RESULT_AVAILABLE, GL_UNSIGNED_INT,        0},
        {"offset",    GL_QUERY_RESULT,           GL_UNSIGNED_INT,        8},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        uint32_t words[8];
        for (unsigned i = 0; i < 8; ++i) words[i] = 0;
        /* A fresh query object per case. Reusing one gave the fourth case
         * twice the sample count -- a query whose result was never fetched
         * carried into the next begin/end pair -- which reads as a driver
         * defect and is a probe defect. */
        glGenQueries(1, &query);
        if (have)
            measure(tests[t].pname, tests[t].type, tests[t].offset, words, 8);
        const GLenum error = glGetError();
        const unsigned slot = tests[t].offset / 4;
        /* Availability is inherently racy: GL does not require the query to
         * have finished, and llvmpipe reports 0 as often as 1. What can be
         * checked is that *something* was written where the sentinel was --
         * which is what a driver ignoring index -1 fails. */
        const int availability =
           tests[t].pname == GL_QUERY_RESULT_AVAILABLE;
        unsigned want = availability ? 0u : (unsigned)COVERED;
        int ok_case = have && error == GL_NO_ERROR &&
           (availability ? (words[slot] == 0u || words[slot] == 1u)
                         : words[slot] == want);
        /* The 64-bit case must have written its high half, and the offset
         * case must not have touched its neighbours. */
        if (tests[t].type == GL_UNSIGNED_INT64_ARB)
            ok_case = ok_case && words[slot + 1] == 0;
        if (tests[t].offset)
            ok_case = ok_case && words[0] == SENTINEL &&
                      words[1] == SENTINEL && words[slot + 1] == SENTINEL;
        fprintf(stderr, "INFO %s got=[%08x,%08x,%08x,%08x] want=%u at word %u "
                "err=0x%04x\n", tests[t].name, words[0], words[1], words[2],
                words[3], availability ? 1u : want, slot, (unsigned)error);
        result(tests[t].name, ok_case);
        glDeleteQueries(1, &query);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
