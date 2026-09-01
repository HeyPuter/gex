/* Primitive restart, against the draws it is shorthand for.
 *
 * WebGPU has restart only for strip topologies and only at a fixed index, so
 * the general GL form is resolved on the CPU: the driver scans the index
 * buffer and re-issues the draw as the ranges between restarts. Two things
 * can go wrong in that, and they interact -- the split itself, and the
 * separate rewrite of a topology WebGPU does not have (TRIANGLE_FAN,
 * LINE_LOOP), which runs first.
 *
 * The check needs no golden image. A restarting draw must produce exactly
 * what the same indices produce as separate draws with the restart taken out,
 * so each topology is rendered both ways and the two framebuffers compared.
 * A topology whose restart is silently ignored shows up as a difference; one
 * that draws nothing at all is caught by the emptiness check.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define SIZE 32
#define RESTART 0xFFFFFFFFu

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); gl_PointSize = 1.0; }\n";

static const char *fragment_source =
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
        char log[512] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

static GLuint index_buffer;

/* Draw `count` indices starting at `first`, as one call. */
static void draw_range(GLenum mode, const GLuint *indices, int first,
                       int count) {
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, (GLsizeiptr)count * 4,
                 indices + first, GL_STREAM_DRAW);
    glDrawElements(mode, count, GL_UNSIGNED_INT, NULL);
}

static int lit_pixels(const unsigned char *pixels) {
    int lit = 0;
    for (int i = 0; i < SIZE * SIZE; ++i)
        if (pixels[i * 4] > 128) ++lit;
    return lit;
}

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
    const EGLint surface_attributes[] = {EGL_WIDTH, SIZE, EGL_HEIGHT, SIZE,
                                         EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIZE, SIZE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "INFO framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, SIZE, SIZE);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    /* Eight positions, none collinear with its neighbours, so that a fan, a
     * strip and a list all cover visibly different areas. */
    static const float positions[16] = {
        -0.9f, -0.9f,   -0.1f, -0.8f,   -0.8f,  0.1f,   -0.2f,  0.7f,
         0.2f, -0.7f,    0.9f, -0.2f,    0.3f,  0.4f,    0.8f,  0.9f,
    };
    GLuint position_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(positions), positions, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glGenBuffers(1, &index_buffer);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, index_buffer);

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) return 2;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[512] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link failed: %s\n", log);
        return 2;
    }
    glUseProgram(program);
    glEnable(GL_PROGRAM_POINT_SIZE);

    /* Two runs of six, and the restart between them. Six because
     * TRIANGLES_ADJACENCY consumes that many per primitive, and a run shorter
     * than one primitive draws nothing and proves nothing. */
    static const GLuint restarting[13] = {0, 1, 2, 3, 4, 5, RESTART,
                                          2, 3, 4, 5, 6, 7};
    static const GLuint firstRun[6] = {0, 1, 2, 3, 4, 5};
    static const GLuint secondRun[6] = {2, 3, 4, 5, 6, 7};

    const struct { const char *name; GLenum mode; } topologies[] = {
        {"points", GL_POINTS},
        {"lines", GL_LINES},
        {"line_strip", GL_LINE_STRIP},
        {"line_loop", GL_LINE_LOOP},
        {"triangles", GL_TRIANGLES},
        {"triangle_strip", GL_TRIANGLE_STRIP},
        {"triangle_fan", GL_TRIANGLE_FAN},
        /* Legal without a geometry shader: the adjacent vertices are simply
         * ignored, and the driver drops them rather than refusing. This is
         * the combination the CTS reaches that the plain topologies do not --
         * a restart split feeding a topology rewrite. */
        {"lines_adjacency", GL_LINES_ADJACENCY},
        {"line_strip_adjacency", GL_LINE_STRIP_ADJACENCY},
        {"triangles_adjacency", GL_TRIANGLES_ADJACENCY},
        {"triangle_strip_adjacency", GL_TRIANGLE_STRIP_ADJACENCY},
    };

    /* Both targets. The CTS checks framebuffer 0 and this probe originally
     * checked only an FBO, which is exactly the difference left over when
     * restart, rasterisation, the viewport and the framebuffer size have all
     * been cleared. */
    for (unsigned target = 0; target < 2; ++target) {
    const int on_window = target == 1;
    glBindFramebuffer(GL_FRAMEBUFFER, on_window ? 0 : framebuffer);
    glViewport(0, 0, SIZE, SIZE);
    for (unsigned t = 0; t < sizeof(topologies) / sizeof(topologies[0]); ++t) {
        unsigned char with_restart[SIZE * SIZE * 4];
        unsigned char as_two_draws[SIZE * SIZE * 4];

        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glEnable(GL_PRIMITIVE_RESTART);
        glPrimitiveRestartIndex(RESTART);
        draw_range(topologies[t].mode, restarting, 0, 13);
        glDisable(GL_PRIMITIVE_RESTART);
        glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, with_restart);

        glClear(GL_COLOR_BUFFER_BIT);
        draw_range(topologies[t].mode, firstRun, 0, 6);
        draw_range(topologies[t].mode, secondRun, 0, 6);
        glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, as_two_draws);

        const GLenum error = glGetError();
        const int lit = lit_pixels(as_two_draws);
        const int same = memcmp(with_restart, as_two_draws,
                                sizeof(with_restart)) == 0;
        /* An all-black pair would compare equal and prove nothing. */
        const int ok = error == GL_NO_ERROR && lit > 0 && same;
        char label[64];
        snprintf(label, sizeof(label), "%s%s", topologies[t].name,
                 on_window ? "_fb0" : "");
        if (!ok)
            fprintf(stderr,
                    "INFO %s restart_lit=%d split_lit=%d same=%d err=0x%04x\n",
                    label, lit_pixels(with_restart), lit, same,
                    (unsigned)error);
        result(label, ok);
    }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
