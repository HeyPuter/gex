/* ARB_transform_feedback3: gl_NextBuffer, and varyings packed into one slot.
 *
 * Two float varyings do not get a varying slot each -- the linker packs them
 * into the components of one, so both arrive at the driver as location 32 and
 * are told apart only by location_frac. Transform feedback then names them
 * separately, so one varying slot produces two xfb outputs whose component
 * masks divide it.
 *
 * A driver that matches an output to a store by location alone therefore finds
 * the wrong one, and every store lands in whichever buffer the first output
 * named -- which is exactly what this driver did until gl_NextBuffer showed
 * it, because one of the two buffers then stays empty rather than merely
 * holding the right bytes for the wrong reason.
 *
 * The interleaved case is the control. The same two varyings captured into a
 * single buffer keep separate slots, so it went on working throughout, and it
 * is here to catch a fix to the packed case that breaks the ordinary one.
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

/* Two separate float outputs, which the linker packs into one slot. */
static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in float value;\n"
    "out float a;\n"
    "out float b;\n"
    "void main() {\n"
    "    a = value;\n"
    "    b = value * 10.0;\n"
    "    gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
    "}\n";

static const char *fragment_source =
    "#version 330 core\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = vec4(1.0); }\n";

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

#define SLOTS 8

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

    int advertised = 0;
    GLint extension_count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &extension_count);
    for (GLint i = 0; i < extension_count && !advertised; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, "GL_ARB_transform_feedback3") == 0)
            advertised = 1;
    }
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO advertised=%d\n", advertised);
    result("xfb3_advertised", advertised);

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) return 2;

    static const float source[3] = {1.0f, 2.0f, 3.0f};
    GLuint source_buffer = 0;
    glGenBuffers(1, &source_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, source_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(source), source, GL_STATIC_DRAW);
    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 1, GL_FLOAT, GL_FALSE, sizeof(float), NULL);

    /* Both cases capture the same two varyings from the same three vertices;
     * only the varying list differs, which is what moves b into its own
     * buffer or leaves it interleaved behind a. */
    const char *split_varyings[3] = {"a", "gl_NextBuffer", "b"};
    const char *packed_varyings[2] = {"a", "b"};
    struct {
        const char *name;
        unsigned varying_count;
        const char **varyings;
        unsigned buffers;
        float want[2][6];
        unsigned want_count[2];
    } tests[] = {
        {"xfb3_next_buffer", 3, split_varyings, 2,
         {{1, 2, 3}, {10, 20, 30}}, {3, 3}},
        {"xfb3_packed_interleaved", 2, packed_varyings, 1,
         {{1, 10, 2, 20, 3, 30}, {0}}, {6, 0}},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        if (!advertised && t == 0) { result(tests[t].name, 0); continue; }
        GLuint program = glCreateProgram();
        glAttachShader(program, vertex);
        glAttachShader(program, fragment);
        glTransformFeedbackVaryings(program, (GLsizei)tests[t].varying_count,
                                    tests[t].varyings, GL_INTERLEAVED_ATTRIBS);
        glLinkProgram(program);
        GLint linked = 0;
        glGetProgramiv(program, GL_LINK_STATUS, &linked);
        if (!linked) {
            char log[1024] = {0};
            glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s link failed: %s\n", tests[t].name, log);
            result(tests[t].name, 0);
            continue;
        }

        float initial[SLOTS];
        for (int i = 0; i < SLOTS; ++i) initial[i] = -1.0f;
        GLuint targets[2] = {0, 0};
        glGenBuffers((GLsizei)tests[t].buffers, targets);
        for (unsigned i = 0; i < tests[t].buffers; ++i) {
            glBindBuffer(GL_ARRAY_BUFFER, targets[i]);
            glBufferData(GL_ARRAY_BUFFER, sizeof(initial), initial,
                         GL_DYNAMIC_COPY);
            glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, i, targets[i]);
        }

        glBindBuffer(GL_ARRAY_BUFFER, source_buffer);
        glVertexAttribPointer(0, 1, GL_FLOAT, GL_FALSE, sizeof(float), NULL);
        glUseProgram(program);
        glEnable(GL_RASTERIZER_DISCARD);
        glBeginTransformFeedback(GL_POINTS);
        glDrawArrays(GL_POINTS, 0, 3);
        glEndTransformFeedback();
        glDisable(GL_RASTERIZER_DISCARD);

        int ok = 1;
        for (unsigned i = 0; i < tests[t].buffers; ++i) {
            float got[SLOTS];
            for (int j = 0; j < SLOTS; ++j) got[j] = -99.0f;
            glBindBuffer(GL_ARRAY_BUFFER, targets[i]);
            glGetBufferSubData(GL_ARRAY_BUFFER, 0, sizeof(got), got);
            const unsigned want_count = tests[t].want_count[i];
            for (unsigned j = 0; j < want_count; ++j)
                if (got[j] != tests[t].want[i][j]) ok = 0;
            /* Nothing past the capture may have been disturbed. */
            if (got[want_count] != -1.0f) ok = 0;
            fprintf(stderr, "INFO %s buffer%u got=", tests[t].name, i);
            for (unsigned j = 0; j < want_count; ++j)
                fprintf(stderr, "%.0f%s", got[j], j + 1 < want_count ? "," : "");
            fprintf(stderr, " next=%.0f want=", got[want_count]);
            for (unsigned j = 0; j < want_count; ++j)
                fprintf(stderr, "%.0f%s", tests[t].want[i][j],
                        j + 1 < want_count ? "," : "");
            fprintf(stderr, " next=-1\n");
        }
        const GLenum error = glGetError();
        if (error != GL_NO_ERROR) {
            fprintf(stderr, "INFO %s err=0x%04x\n", tests[t].name,
                    (unsigned)error);
            ok = 0;
        }
        result(tests[t].name, ok);
        glDeleteBuffers((GLsizei)tests[t].buffers, targets);
        glDeleteProgram(program);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
