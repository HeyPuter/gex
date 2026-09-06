/* Wide points, which WebGPU cannot draw and the driver expands into quads.
 *
 * A WebGPU point is one pixel and WGSL has no point_size builtin, so
 * glPointSize did nothing at all: `spriteblast` and `points` were the two
 * largest divergences in the whole llvmpipe comparison because a particle
 * system drawn as single pixels shares no structure with the same one drawn
 * as sprites. The driver now rewrites a wide-point draw as six vertices by
 * one instance per point -- see gex_wgpu_lower_wide_point.
 *
 * Four things can be independently wrong once that machinery exists, and this
 * probe separates them:
 *
 *  - the *extent*: is the point the right number of pixels across, centred on
 *    the vertex, in both axes? A quad built from a mis-scaled offset still
 *    draws something, and a viewport-versus-framebuffer mistake only shows up
 *    when the two differ.
 *  - gl_PointCoord: does it vary across the point, and which way up? A
 *    constant sprite coordinate draws a solid square that looks like a
 *    correctly expanded point until you check the colours.
 *  - gl_PointSize written by the shader, which arrives on a different path
 *    from the glPointSize state.
 *  - the vertex's own attributes: with the per-point data moved to the
 *    instance index, a point that reads its colour from an array is the case
 *    that says the inversion is wired up.
 *
 * Everything is checked from read-back pixels rather than state queries: the
 * expansion happens below GL, so glGet cannot see it.
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

#define SIZE 32
static unsigned char pixels[SIZE * SIZE * 4];

static void read_target(void) {
    memset(pixels, 0xAB, sizeof(pixels));
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
}
static unsigned char channel(unsigned x, unsigned y, unsigned c) {
    return pixels[(y * SIZE + x) * 4 + c];
}
static int lit(unsigned x, unsigned y) {
    return channel(x, y, 0) > 40 || channel(x, y, 1) > 40 ||
           channel(x, y, 2) > 40;
}
static void clear_black(void) {
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
}

/* The lit bounding box, which is what says how wide the point came out. */
static void lit_bounds(int *x0, int *y0, int *x1, int *y1) {
    *x0 = *y0 = SIZE;
    *x1 = *y1 = -1;
    for (unsigned y = 0; y < SIZE; ++y)
        for (unsigned x = 0; x < SIZE; ++x)
            if (lit(x, y)) {
                if ((int)x < *x0) *x0 = x;
                if ((int)y < *y0) *y0 = y;
                if ((int)x > *x1) *x1 = x;
                if ((int)y > *y1) *y1 = y;
            }
}

static GLuint compile(GLenum stage, const char *source) {
    const GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "shader failed: %s\n", log);
        return 0;
    }
    return shader;
}

static GLuint program_from(const char *vertex, const char *fragment) {
    const GLuint vs = compile(GL_VERTEX_SHADER, vertex);
    const GLuint fs = compile(GL_FRAGMENT_SHADER, fragment);
    if (!vs || !fs) return 0;
    const GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint ok = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "link failed: %s\n", log);
        return 0;
    }
    return program;
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
    const EGLint context_attributes[] = {EGL_NONE};
    const EGLint surface_attributes[] = {EGL_WIDTH, SIZE, EGL_HEIGHT, SIZE,
                                         EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

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
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, SIZE, SIZE);
    glPixelStorei(GL_PACK_ALIGNMENT, 1);

    /* 1. Extent. One white point of 8 pixels at the centre of the target, on
     * a viewport that is the whole framebuffer. GL centres the point on the
     * vertex, so a size of 8 covers 8 pixels each way -- and a driver that
     * halved or doubled the offset, or divided by the framebuffer where it
     * should have divided by the viewport, lands outside the tolerance. */
    {
        clear_black();
        glColor3f(1.0f, 1.0f, 1.0f);
        glPointSize(8.0f);
        glBegin(GL_POINTS);
        glVertex2f(0.0f, 0.0f);
        glEnd();
        read_target();
        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        const int width = x1 - x0 + 1, height = y1 - y0 + 1;
        /* One pixel of slack each way: where the edge of an 8-pixel point
         * centred on a pixel corner falls is a fill-rule question, and this
         * is not a rasterisation conformance test. */
        const int ok = glGetError() == GL_NO_ERROR &&
                       width >= 7 && width <= 9 &&
                       height >= 7 && height <= 9 &&
                       x0 >= SIZE / 2 - 5 && x1 <= SIZE / 2 + 4 &&
                       y0 >= SIZE / 2 - 5 && y1 <= SIZE / 2 + 4;
        fprintf(stderr, "INFO extent box=%d,%d..%d,%d %dx%d\n",
                x0, y0, x1, y1, width, height);
        result("extent", ok);
    }

    /* 1b. A point of the default size, which is one pixel.
     *
     * Everything else here draws a *wide* point, and a wide point is expanded
     * into a quad by the driver -- so the whole of this probe was testing the
     * expansion and none of it the plain rasterised point WebGPU draws when
     * no expansion is wanted. A one-pixel point is what a particle system, a
     * point cloud or a debug overlay draws. */
    {
        clear_black();
        glColor3f(1.0f, 1.0f, 1.0f);
        glPointSize(1.0f);
        glBegin(GL_POINTS);
        /* Not the exact centre: on an even viewport that is a pixel corner,
         * and whether a one-pixel point covers anything there is a fill-rule
         * question rather than a fact about the driver. */
        glVertex2f(0.1234f, 0.1234f);
        glEnd();
        read_target();
        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        const int lit = x0 <= x1 && y0 <= y1;
        fprintf(stderr, "INFO single_pixel lit=%d box=%d,%d..%d,%d\n", lit,
                x0, y0, x1, y1);
        result("single_pixel", glGetError() == GL_NO_ERROR && lit &&
                               x1 - x0 <= 1 && y1 - y0 <= 1);
    }

    /* 2. A viewport smaller than the framebuffer, which is what tells a
     * viewport-relative offset from a framebuffer-relative one. Half the
     * width means clip space spans half as many pixels, so the same size in
     * pixels is twice the offset in clip space -- get it wrong and the point
     * comes out half or double. */
    {
        glViewport(0, 0, SIZE / 2, SIZE / 2);
        clear_black();
        glPointSize(8.0f);
        glBegin(GL_POINTS);
        glVertex2f(0.0f, 0.0f);
        glEnd();
        read_target();
        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        const int width = x1 - x0 + 1, height = y1 - y0 + 1;
        const int ok = glGetError() == GL_NO_ERROR &&
                       width >= 7 && width <= 9 && height >= 7 && height <= 9;
        fprintf(stderr, "INFO half-viewport box=%d,%d..%d,%d %dx%d\n",
                x0, y0, x1, y1, width, height);
        result("half_viewport_extent", ok);
        glViewport(0, 0, SIZE, SIZE);
    }

    /* 3. gl_PointCoord across one large point: red is s and green is t, so
     * the four quadrants of the point are four different colours and both the
     * direction and the orientation are readable.
     *
     * Which way t runs is measured against llvmpipe rather than asserted from
     * the spec text. GL_POINT_SPRITE_COORD_ORIGIN defaults to GL_UPPER_LEFT
     * and is defined in *window* coordinates, and this probe renders to an
     * FBO, whose y runs the other way -- so on this target t grows towards
     * the larger read-back row, which is what Mesa produces and what the
     * driver has to match. A probe that only checked that t varies would pass
     * on a mirrored sprite, which is why the direction is here at all. */
    {
        const char *vertex =
            "#version 130\n"
            "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
        const char *fragment =
            "#version 130\n"
            "void main() {\n"
            "  gl_FragColor = vec4(gl_PointCoord.x, gl_PointCoord.y, 0.5, 1.0);\n"
            "}\n";
        const GLuint program = program_from(vertex, fragment);
        if (!program) {
            result("point_coord_varies", 0);
            result("point_coord_orientation", 0);
        } else {
            clear_black();
            glUseProgram(program);
            glPointSize(20.0f);
            glDrawArrays(GL_POINTS, 0, 1);
            read_target();
            glUseProgram(0);
            int x0, y0, x1, y1;
            lit_bounds(&x0, &y0, &x1, &y1);
            /* Sampled inside the point rather than at its edge, where the
             * coordinate is near 0 or 1 and a half-pixel of fill rule is a
             * large fraction of the difference being measured. */
            const unsigned left = x0 + 3, right = x1 - 3;
            const unsigned low = y0 + 3, high = y1 - 3;
            const int varies_s = channel(right, low, 0) >
                                 channel(left, low, 0) + 60;
            const int varies_t = channel(left, low, 1) + 60 <
                                 channel(left, high, 1);
            fprintf(stderr, "INFO pointcoord box=%d,%d..%d,%d "
                    "left=%u,%u right=%u,%u low=%u,%u high=%u,%u\n",
                    x0, y0, x1, y1,
                    channel(left, low, 0), channel(left, low, 1),
                    channel(right, low, 0), channel(right, low, 1),
                    channel(left, low, 0), channel(left, low, 1),
                    channel(left, high, 0), channel(left, high, 1));
            result("point_coord_varies",
                   glGetError() == GL_NO_ERROR && varies_s &&
                   channel(left, high, 0) + 60 < channel(right, high, 0));
            /* s grows with x, and t with the read-back row. Both
             * directions, because a probe that only checked that the value
             * varies would pass on a mirrored sprite. */
            result("point_coord_orientation", varies_t);
        }
    }

    /* 4. gl_PointSize written by the vertex shader, which is the other way a
     * point gets its width and reaches the driver through the shader rather
     * than the rasterizer state. Two points at different sizes in one draw,
     * so a driver taking the size from the wrong invocation -- or from a
     * temporary that had not been written yet -- draws them the same. */
    {
        const char *vertex =
            "#version 130\n"
            "in vec2 position;\n"
            "in float want_size;\n"
            "void main() {\n"
            "  gl_Position = vec4(position, 0.0, 1.0);\n"
            "  gl_PointSize = want_size;\n"
            "}\n";
        const char *fragment =
            "#version 130\n"
            "void main() { gl_FragColor = vec4(1.0); }\n";
        const GLuint program = program_from(vertex, fragment);
        if (!program) {
            result("shader_point_size", 0);
        } else {
            /* Left point small, right point large. */
            static const float vertices[2][3] = {
                {-0.5f, 0.0f, 4.0f},
                { 0.5f, 0.0f, 14.0f},
            };
            GLuint buffer = 0, array = 0;
            glGenVertexArrays(1, &array);
            glBindVertexArray(array);
            glGenBuffers(1, &buffer);
            glBindBuffer(GL_ARRAY_BUFFER, buffer);
            glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices,
                         GL_STATIC_DRAW);
            const GLint position = glGetAttribLocation(program, "position");
            const GLint size = glGetAttribLocation(program, "want_size");
            glEnableVertexAttribArray(position);
            glVertexAttribPointer(position, 2, GL_FLOAT, GL_FALSE,
                                  3 * sizeof(float), (void *)0);
            glEnableVertexAttribArray(size);
            glVertexAttribPointer(size, 1, GL_FLOAT, GL_FALSE,
                                  3 * sizeof(float),
                                  (void *)(2 * sizeof(float)));
            clear_black();
            glUseProgram(program);
            glEnable(GL_PROGRAM_POINT_SIZE);
            glDrawArrays(GL_POINTS, 0, 2);
            glDisable(GL_PROGRAM_POINT_SIZE);
            read_target();
            glUseProgram(0);
            /* Each point's own height, measured in its own half of the
             * target: the two differ by more than any fill rule. */
            int small_rows = 0, large_rows = 0;
            for (unsigned y = 0; y < SIZE; ++y) {
                small_rows += lit(SIZE / 4, y);
                large_rows += lit(3 * SIZE / 4, y);
            }
            fprintf(stderr, "INFO shader size small=%d large=%d err=0x%04x\n",
                    small_rows, large_rows, (unsigned)glGetError());
            result("shader_point_size",
                   small_rows >= 3 && small_rows <= 5 &&
                   large_rows >= 13 && large_rows <= 15);
            glBindVertexArray(0);
            glDeleteBuffers(1, &buffer);
            glDeleteVertexArrays(1, &array);
        }
    }

    /* 5. Per-point attributes, which the expansion moves onto the instance
     * index. Two points with different colours from a vertex array: if the
     * step mode were left per-vertex, all six corners of the first quad would
     * read the first point and the second would read past the array. */
    {
        static const float vertices[2][5] = {
            {-0.5f, 0.0f, 1.0f, 0.0f, 0.0f},
            { 0.5f, 0.0f, 0.0f, 1.0f, 0.0f},
        };
        clear_black();
        glPointSize(6.0f);
        glEnableClientState(GL_VERTEX_ARRAY);
        glEnableClientState(GL_COLOR_ARRAY);
        glVertexPointer(2, GL_FLOAT, 5 * sizeof(float), vertices);
        glColorPointer(3, GL_FLOAT, 5 * sizeof(float), &vertices[0][2]);
        glDrawArrays(GL_POINTS, 0, 2);
        glDisableClientState(GL_COLOR_ARRAY);
        glDisableClientState(GL_VERTEX_ARRAY);
        read_target();
        const unsigned left_x = SIZE / 4, right_x = 3 * SIZE / 4;
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(left_x, SIZE / 2, 0) > 200 &&
                       channel(left_x, SIZE / 2, 1) < 60 &&
                       channel(right_x, SIZE / 2, 1) > 200 &&
                       channel(right_x, SIZE / 2, 0) < 60;
        fprintf(stderr, "INFO per-point left=%u,%u,%u right=%u,%u,%u\n",
                channel(left_x, SIZE / 2, 0), channel(left_x, SIZE / 2, 1),
                channel(left_x, SIZE / 2, 2), channel(right_x, SIZE / 2, 0),
                channel(right_x, SIZE / 2, 1), channel(right_x, SIZE / 2, 2));
        result("per_point_attributes", ok);
    }

    /* 6. And a point of size 1, which must stay one pixel: the expansion is
     * skipped there, and a quad built for it anyway would cover four. */
    {
        clear_black();
        glColor3f(1.0f, 1.0f, 1.0f);
        glPointSize(1.0f);
        glBegin(GL_POINTS);
        glVertex2f(0.0f, 0.0f);
        glEnd();
        read_target();
        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        fprintf(stderr, "INFO single box=%d,%d..%d,%d\n", x0, y0, x1, y1);
        result("size_one_is_one_pixel",
               glGetError() == GL_NO_ERROR && x0 == x1 && y0 == y1);
    }

    /* 7. GL_COORD_REPLACE, which is the fixed-function way to the same
     * coordinate and a *different* path through the driver: the fragment
     * shader reads an ordinary texcoord varying and the rasteriser is
     * supposed to override it with the sprite coordinate. st/mesa's own
     * fragment shader does write that varying, so a driver substituting only
     * where nothing produces it leaves the point sampling one texel --
     * `point-sprite` drew a solid square of whichever texel that was.
     *
     * A 2x2 texture with four different colours, so each quadrant of the
     * point lands on its own texel and both axes are readable. */
    {
        static const unsigned char texels[4][4] = {
            {255, 0, 0, 255}, {0, 255, 0, 255},     /* first row */
            {0, 0, 255, 255}, {255, 255, 0, 255},   /* second row */
        };
        GLuint texture = 0;
        glGenTextures(1, &texture);
        glBindTexture(GL_TEXTURE_2D, texture);
        glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 2, 0, GL_RGBA,
                     GL_UNSIGNED_BYTE, texels);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        glEnable(GL_TEXTURE_2D);
        glEnable(GL_POINT_SPRITE);
        glTexEnvi(GL_POINT_SPRITE, GL_COORD_REPLACE, GL_TRUE);
        clear_black();
        glColor3f(1.0f, 1.0f, 1.0f);
        glPointSize(20.0f);
        glBegin(GL_POINTS);
        glVertex2f(0.0f, 0.0f);
        glEnd();
        read_target();
        const GLenum error = glGetError();
        glDisable(GL_POINT_SPRITE);
        glDisable(GL_TEXTURE_2D);
        glDeleteTextures(1, &texture);

        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        const unsigned left = x0 + 3, right = x1 - 3;
        const unsigned low = y0 + 3, high = y1 - 3;
        /* Four samples, one per quadrant. Which colour is where depends on
         * the sprite origin, so what is asserted is that the four differ --
         * and separately that the two axes each change something, which a
         * constant coordinate cannot imitate. */
        const int red_varies =
            (channel(left, low, 0) > 128) != (channel(right, low, 0) > 128) ||
            (channel(left, high, 0) > 128) != (channel(right, high, 0) > 128);
        const int blue_varies =
            (channel(left, low, 2) > 128) != (channel(left, high, 2) > 128) ||
            (channel(right, low, 2) > 128) != (channel(right, high, 2) > 128);
        fprintf(stderr, "INFO coord_replace box=%d,%d..%d,%d "
                "ll=%u,%u,%u lr=%u,%u,%u ul=%u,%u,%u ur=%u,%u,%u\n",
                x0, y0, x1, y1,
                channel(left, low, 0), channel(left, low, 1),
                channel(left, low, 2),
                channel(right, low, 0), channel(right, low, 1),
                channel(right, low, 2),
                channel(left, high, 0), channel(left, high, 1),
                channel(left, high, 2),
                channel(right, high, 0), channel(right, high, 1),
                channel(right, high, 2));
        result("coord_replace_varies",
               error == GL_NO_ERROR && red_varies && blue_varies);
    }

    /* 8. An *indexed* point draw, which the expansion cannot take directly:
     * it needs the per-point data on the instance index and WebGPU's index
     * buffer addresses only the vertex one. The driver de-indexes the draw
     * into a gathered vertex buffer instead
     * (gex_wgpu_gather_indexed_vertices), and KHR-GL33's
     * primitive_restart.restart_mode is exactly this shape -- indexed points
     * from a shader writing gl_PointSize = 4.0.
     *
     * The indices are deliberately out of order and skip a vertex, because a
     * gather that ignored them and copied straight through would put the
     * right number of points in the wrong places with the wrong colours. */
    {
        static const float vertices[4][5] = {
            {-0.6f, -0.6f,  1.0f, 0.0f, 0.0f},   /* 0: red, lower left */
            { 0.0f,  0.0f,  1.0f, 1.0f, 1.0f},   /* 1: white, centre -- unused */
            { 0.6f,  0.6f,  0.0f, 1.0f, 0.0f},   /* 2: green, upper right */
            {-0.6f,  0.6f,  0.0f, 0.0f, 1.0f},   /* 3: blue, upper left */
        };
        static const unsigned short indices[2] = {2, 3};
        clear_black();
        glPointSize(6.0f);
        glEnableClientState(GL_VERTEX_ARRAY);
        glEnableClientState(GL_COLOR_ARRAY);
        glVertexPointer(2, GL_FLOAT, 5 * sizeof(float), vertices);
        glColorPointer(3, GL_FLOAT, 5 * sizeof(float), &vertices[0][2]);
        glDrawElements(GL_POINTS, 2, GL_UNSIGNED_SHORT, indices);
        glDisableClientState(GL_COLOR_ARRAY);
        glDisableClientState(GL_VERTEX_ARRAY);
        read_target();
        const GLenum error = glGetError();
        /* Upper right is green (vertex 2), upper left blue (vertex 3), and
         * the centre stays black -- vertex 1 is not indexed, and a gather
         * that copied the first two vertices instead would light it. */
        const unsigned right_x = (unsigned)((0.6f + 1.0f) * 0.5f * SIZE);
        const unsigned left_x = (unsigned)((-0.6f + 1.0f) * 0.5f * SIZE);
        const unsigned top_y = (unsigned)((0.6f + 1.0f) * 0.5f * SIZE);
        const int ok = error == GL_NO_ERROR &&
                       channel(right_x, top_y, 1) > 200 &&
                       channel(right_x, top_y, 2) < 60 &&
                       channel(left_x, top_y, 2) > 200 &&
                       channel(left_x, top_y, 1) < 60 &&
                       !lit(SIZE / 2, SIZE / 2);
        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        fprintf(stderr, "INFO indexed box=%d,%d..%d,%d right=%u,%u,%u "
                "left=%u,%u,%u centre=%d err=0x%04x\n", x0, y0, x1, y1,
                channel(right_x, top_y, 0), channel(right_x, top_y, 1),
                channel(right_x, top_y, 2), channel(left_x, top_y, 0),
                channel(left_x, top_y, 1), channel(left_x, top_y, 2),
                lit(SIZE / 2, SIZE / 2), (unsigned)error);
        result("indexed_points", ok);
    }

    /* 9. The same indexed draw at size 1, which does *not* go through the
     * expansion. It is here to tell a defect in the expansion apart from one
     * in the indexed client-array path underneath it: if both cases fail the
     * gather is innocent. */
    {
        static const float vertices[4][5] = {
            {-0.6f, -0.6f,  1.0f, 0.0f, 0.0f},
            { 0.0f,  0.0f,  1.0f, 1.0f, 1.0f},
            { 0.6f,  0.6f,  0.0f, 1.0f, 0.0f},
            {-0.6f,  0.6f,  0.0f, 0.0f, 1.0f},
        };
        static const unsigned short indices[2] = {2, 3};
        clear_black();
        glPointSize(1.0f);
        glEnableClientState(GL_VERTEX_ARRAY);
        glEnableClientState(GL_COLOR_ARRAY);
        glVertexPointer(2, GL_FLOAT, 5 * sizeof(float), vertices);
        glColorPointer(3, GL_FLOAT, 5 * sizeof(float), &vertices[0][2]);
        glDrawElements(GL_POINTS, 2, GL_UNSIGNED_SHORT, indices);
        glDisableClientState(GL_COLOR_ARRAY);
        glDisableClientState(GL_VERTEX_ARRAY);
        read_target();
        const GLenum error = glGetError();
        int x0, y0, x1, y1;
        lit_bounds(&x0, &y0, &x1, &y1);
        /* Two lit pixels, in the upper half, and none in the middle. Their
         * exact pixel is a fill-rule question at these coordinates, so the
         * box is what is checked rather than a sample. */
        int lit_count = 0;
        for (unsigned y = 0; y < SIZE; ++y)
            for (unsigned x = 0; x < SIZE; ++x)
                lit_count += lit(x, y);
        fprintf(stderr, "INFO indexed_thin box=%d,%d..%d,%d lit=%d "
                "err=0x%04x\n", x0, y0, x1, y1, lit_count, (unsigned)error);
        result("indexed_points_size_one",
               error == GL_NO_ERROR && lit_count == 2 &&
               y0 > (int)SIZE / 2 && x0 < (int)SIZE / 2 &&
               x1 > (int)SIZE / 2);
    }

    /* 10. The same draw again through glDrawRangeElements, which is the form
     * GL 1.2 added and most old code actually calls. It is here because the
     * guest transports `indices` for the whole DrawElements family as an
     * *element-buffer offset* -- correct in a core context, where it always
     * is one, and wrong in the compatibility context the guest now hands out,
     * where it can be a client pointer. glDrawElements itself is
     * hand-written and handles both; the rest of the family is generated and
     * does not. */
    {
        static const float vertices[4][5] = {
            {-0.6f, -0.6f,  1.0f, 0.0f, 0.0f},
            { 0.0f,  0.0f,  1.0f, 1.0f, 1.0f},
            { 0.6f,  0.6f,  0.0f, 1.0f, 0.0f},
            {-0.6f,  0.6f,  0.0f, 0.0f, 1.0f},
        };
        static const unsigned short indices[2] = {2, 3};
        clear_black();
        glPointSize(6.0f);
        glEnableClientState(GL_VERTEX_ARRAY);
        glEnableClientState(GL_COLOR_ARRAY);
        glVertexPointer(2, GL_FLOAT, 5 * sizeof(float), vertices);
        glColorPointer(3, GL_FLOAT, 5 * sizeof(float), &vertices[0][2]);
        glDrawRangeElements(GL_POINTS, 2, 3, 2, GL_UNSIGNED_SHORT, indices);
        glDisableClientState(GL_COLOR_ARRAY);
        glDisableClientState(GL_VERTEX_ARRAY);
        read_target();
        const GLenum error = glGetError();
        const unsigned right_x = (unsigned)((0.6f + 1.0f) * 0.5f * SIZE);
        const unsigned left_x = (unsigned)((-0.6f + 1.0f) * 0.5f * SIZE);
        const unsigned top_y = (unsigned)((0.6f + 1.0f) * 0.5f * SIZE);
        const int ok = error == GL_NO_ERROR &&
                       channel(right_x, top_y, 1) > 200 &&
                       channel(left_x, top_y, 2) > 200 &&
                       !lit(SIZE / 2, SIZE / 2);
        fprintf(stderr, "INFO range right=%u,%u,%u left=%u,%u,%u centre=%d "
                "err=0x%04x\n",
                channel(right_x, top_y, 0), channel(right_x, top_y, 1),
                channel(right_x, top_y, 2), channel(left_x, top_y, 0),
                channel(left_x, top_y, 1), channel(left_x, top_y, 2),
                lit(SIZE / 2, SIZE / 2), (unsigned)error);
        result("draw_range_elements", ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
