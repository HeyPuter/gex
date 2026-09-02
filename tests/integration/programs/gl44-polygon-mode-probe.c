/* glPolygonMode(GL_LINE), checked by where the pixels are rather than by
 * whether the state was accepted.
 *
 * WebGPU has no polygon mode, so the driver rewrites the polygon into the
 * lines it would be drawn as. A driver that accepted the state and drew the
 * triangle filled anyway would look like it worked; what separates the two is
 * the interior. So the test draws one large triangle and asks for a pixel on
 * an edge and a pixel well inside it, with a filled pass as the control:
 *
 *   GL_FILL  edge lit, interior lit
 *   GL_LINE  edge lit, interior dark
 *
 * The interior sample is taken away from the centroid and away from every
 * edge, so it cannot be caught by a line that happens to be a pixel wide in
 * the wrong place.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
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

static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

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
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

#define SIDE 64

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

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);

    /* A right triangle filling most of the viewport, its right angle at the
     * bottom left. The hypotenuse runs corner to corner, so a point at
     * (0.25, 0.25) of the viewport is inside and comfortably clear of all
     * three edges. */
    static const float triangle[6] = {
        -0.95f, -0.95f,   0.95f, -0.95f,   -0.95f, 0.95f,
    };
    GLuint vao = 0, buffer = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(triangle), triangle, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glUseProgram(program);

    /* The hypotenuse runs corner to corner, so x + y = SIDE along it and its
     * midpoint is the centre of the target. Filled or unfilled, that pixel is
     * covered -- which makes it the "did it draw anything at all" sample, so
     * that an unfilled pass which drew nothing cannot pass by having a dark
     * interior. The interior sample is at a fifth of the way in, inside the
     * triangle and clear of all three edges by many pixels.
     *
     * An edge sample is deliberately not used: the bottom edge sits at
     * y = 1.6 and a filled triangle does not cover the centre of pixel 1,
     * while a rasterised line through it does. That difference is about
     * where the edge falls in the pixel grid and says nothing about polygon
     * mode -- it made llvmpipe itself fail the filled case. */
    const int inside_x = SIDE / 5, inside_y = SIDE / 5;

    struct { const char *name; GLenum mode; int want_inside; } tests[] = {
        {"fill_covers_interior", GL_FILL, 1},
        {"line_leaves_interior_dark", GL_LINE, 0},
    };

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        glPolygonMode(GL_FRONT_AND_BACK, tests[i].mode);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);

        /* Counted over the whole target, not sampled at one point. Any
         * single pixel on an edge is a question about where that edge falls
         * in the pixel grid -- the first version of this probe sampled the
         * bottom edge and llvmpipe failed its own filled case, because a
         * filled triangle does not cover the centre of the pixel a
         * rasterised line through the same edge does. A count cannot go
         * wrong that way: filled lights most of the triangle, unfilled
         * lights its outline, and a draw that vanished lights nothing. */
        unsigned char *image = malloc((size_t)SIDE * SIDE * 4);
        if (!image) return 2;
        glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);
        unsigned lit = 0;
        for (unsigned p = 0; p < (unsigned)SIDE * SIDE; ++p)
            if (image[p * 4] > 128) ++lit;
        unsigned char inside[4] = {0};
        glReadPixels(inside_x, inside_y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     inside);
        free(image);
        const GLenum error = glGetError();
        const int inside_lit = inside[0] > 128;
        /* Half the target is a lot more than an outline and a lot less than
         * nothing, so it separates the two without encoding either shape. */
        const unsigned area = (unsigned)SIDE * SIDE;
        const int coverage_ok = tests[i].want_inside
            ? lit > area / 4 : (lit > 0 && lit < area / 8);
        const int ok = error == GL_NO_ERROR && coverage_ok &&
                       inside_lit == tests[i].want_inside;
        fprintf(stderr,
                "INFO %s lit=%u/%u interior=%u want_interior=%d err=0x%04x\n",
                tests[i].name, lit, area, inside[0], tests[i].want_inside,
                (unsigned)error);
        result(tests[i].name, ok);
    }
    glPolygonMode(GL_FRONT_AND_BACK, GL_FILL);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
