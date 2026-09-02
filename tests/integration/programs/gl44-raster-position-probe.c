/* Where a point and a thin line actually land.
 *
 * KHR-GL33.primitive_restart checks spot positions in the default
 * framebuffer, and after the framebuffer-size fix its POINTS, LINES and
 * LINE_STRIP sub-cases still fail while every triangle topology passes. That
 * shape -- thin primitives only -- points at rasterisation rather than at
 * anything to do with restart, which a separate probe has already cleared.
 *
 * This measures it. Single points at exact pixel centres, and one horizontal
 * line, rendered into an FBO so the readback is well defined, with the lit
 * pixels reported. Whether the browser agrees with native Mesa is the whole
 * question; the CTS's own check is the same one at a different address.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define SIZE 8

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
    "void main() { fragColour = vec4(1.0); }\n";

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

/* The centre of pixel `i` along an axis of SIZE, in normalised device
 * coordinates -- so a point placed here has no rounding to resolve. */
static float pixel_centre(int i) {
    return 2.0f * ((float)i + 0.5f) / (float)SIZE - 1.0f;
}

/* A compact description of which pixels are lit, so a difference is legible
 * rather than a byte count: pixels listed bottom row first, GL's order. */
static void describe(const char *label, const unsigned char *pixels) {
    char text[SIZE * SIZE * 8 + 1];
    size_t at = 0;
    int lit = 0;
    for (int y = 0; y < SIZE; ++y)
        for (int x = 0; x < SIZE; ++x)
            if (pixels[(y * SIZE + x) * 4] > 128) {
                ++lit;
                if (at < sizeof(text) - 8)
                    at += (size_t)snprintf(text + at, 8, "%d,%d ", x, y);
            }
    text[at] = 0;
    fprintf(stderr, "INFO %s lit=%d at %s\n", label, lit, text);
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

    GLuint vao = 0, buffer = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) return 2;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) return 2;
    glUseProgram(program);
    glEnable(GL_PROGRAM_POINT_SIZE);

    unsigned char pixels[SIZE * SIZE * 4];
    static const int points[][2] = {{0, 0}, {3, 5}, {7, 7}, {1, 6}};

    for (unsigned i = 0; i < sizeof(points) / sizeof(points[0]); ++i) {
        const float position[2] = {pixel_centre(points[i][0]),
                                   pixel_centre(points[i][1])};
        glBufferData(GL_ARRAY_BUFFER, sizeof(position), position,
                     GL_STREAM_DRAW);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_POINTS, 0, 1);
        glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
        char label[32];
        snprintf(label, sizeof(label), "point_%d_%d", points[i][0],
                 points[i][1]);
        describe(label, pixels);
        /* Exactly the pixel asked for, and nothing else. */
        int ok = 1;
        for (int y = 0; y < SIZE; ++y)
            for (int x = 0; x < SIZE; ++x) {
                const int want = (x == points[i][0] && y == points[i][1]);
                const int got = pixels[(y * SIZE + x) * 4] > 128;
                if (want != got) ok = 0;
            }
        result(label, ok);
    }

    /* A horizontal line along row 3, from pixel 1 to pixel 6. GL excludes the
     * final endpoint, so five pixels are expected -- but what matters here is
     * whether the browser and native Mesa agree, which is what the gate
     * compares. */
    const float line[4] = {pixel_centre(1), pixel_centre(3),
                           pixel_centre(6), pixel_centre(3)};
    glBufferData(GL_ARRAY_BUFFER, sizeof(line), line, GL_STREAM_DRAW);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_LINES, 0, 2);
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    describe("line_row3", pixels);
    int line_ok = 1;
    for (int y = 0; y < SIZE; ++y)
        for (int x = 0; x < SIZE; ++x) {
            const int want = (y == 3 && x >= 1 && x <= 5);
            const int got = pixels[(y * SIZE + x) * 4] > 128;
            if (want != got) line_ok = 0;
        }
    result("line_row3", line_ok);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
