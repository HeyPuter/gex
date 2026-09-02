/* glPolygonStipple, which WebGPU has no form of at all.
 *
 * The pattern is applied as a discard in a fragment variant, with the 32
 * pattern words baked in as immediates. Two things about that are easy to get
 * wrong and invisible in a one-row test:
 *
 *  - the column order. glPolygonStipple's bit 31 of each word is the leftmost
 *    pixel, not bit 0.
 *  - the row order. GL anchors the pattern at the window origin, which is the
 *    *bottom*, while WebGPU's fragment position counts from the top -- so
 *    whether the row is `y & 31` or `(height - 1 - y) & 31` depends on
 *    something the driver does elsewhere, and getting it wrong is a vertical
 *    flip whose phase depends on the framebuffer height.
 *
 * So the target is 32x32, the full period of the pattern in both axes, and the
 * row pattern is checked again on a 48-tall target: the two formulas differ
 * only by an inversion at 32 and by an inversion plus a shift at 48, so one
 * height alone cannot tell them apart.
 *
 * glPolygonStipple is compatibility-profile only, which an empty EGL context
 * attribute list asks for.
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

static const char *vertex_source =
    "attribute vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *fragment_source =
    "void main() { gl_FragColor = vec4(1.0, 1.0, 1.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *source, const char *name) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s compile failed: %s\n", name, log);
        return 0;
    }
    return shader;
}

/* Read the 32x32 target back and report the pixels at (x, y), GL's convention
 * with y counted from the bottom. */
static unsigned char pixels[32 * 32 * 4];

static unsigned char at(unsigned x, unsigned y) {
    return pixels[(y * 32 + x) * 4];
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 32, 32, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 32, 32);

    static const float quad[8] = {
        -1.0f, -1.0f,  1.0f, -1.0f,  -1.0f, 1.0f,  1.0f, 1.0f,
    };
    GLuint position_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source, "vertex");
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source, "fragment");
    if (!vertex || !fragment) {
        result("legacy_shaders", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
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
        result("legacy_shaders", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
    result("legacy_shaders", 1);
    glUseProgram(program);
    const GLint position = glGetAttribLocation(program, "pos");
    glEnableVertexAttribArray(position);
    glVertexAttribPointer(position, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    /* glPolygonStipple's array is 32 rows of 4 bytes, the first row being the
     * bottom one, and within a byte the most significant bit is the leftmost
     * pixel. */
    unsigned char columns[128];
    unsigned char rows[128];
    for (int row = 0; row < 32; ++row) {
        for (int byte = 0; byte < 4; ++byte) {
            /* 0xAA: bits 7, 5, 3, 1 of each byte, so pixels 0, 2, 4, 6. */
            columns[row * 4 + byte] = 0xAA;
            rows[row * 4 + byte] = (row % 2) == 0 ? 0xFF : 0x00;
        }
    }

    struct { const char *name; const unsigned char *pattern; } tests[] = {
        {"columns", columns},
        {"rows", rows},
    };

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        glPolygonStipple(tests[i].pattern);
        glEnable(GL_POLYGON_STIPPLE);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        memset(pixels, 0xAB, sizeof(pixels));
        glReadPixels(0, 0, 32, 32, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
        glDisable(GL_POLYGON_STIPPLE);
        const GLenum error = glGetError();

        int ok = error == GL_NO_ERROR;
        if (i == 0) {
            /* Even columns lit, odd columns clear, in four different rows. */
            for (unsigned y = 0; y < 32; y += 8) {
                for (unsigned x = 0; x < 32; ++x) {
                    const int lit = at(x, y) > 128;
                    if (lit != ((x % 2) == 0)) ok = 0;
                }
            }
            fprintf(stderr, "INFO columns row0=%u,%u,%u,%u err=0x%04x\n",
                    at(0, 0), at(1, 0), at(2, 0), at(3, 0), (unsigned)error);
        } else {
            /* Even rows lit counting from the bottom, in four columns. */
            for (unsigned y = 0; y < 32; ++y) {
                for (unsigned x = 0; x < 32; x += 8) {
                    const int lit = at(x, y) > 128;
                    if (lit != ((y % 2) == 0)) ok = 0;
                }
            }
            fprintf(stderr, "INFO rows col0=%u,%u,%u,%u err=0x%04x\n",
                    at(0, 0), at(0, 1), at(0, 2), at(0, 3), (unsigned)error);
        }
        result(tests[i].name, ok);
    }

    /* The row pattern again on a 48-pixel-tall target.
     *
     * This is what separates "the fragment coordinate is bottom-up, so the
     * row is y & 31" from "it is top-down, so the row is (height - 1 - y) & 31
     * and the height sets the phase". At height 32 the two differ only by an
     * inversion; at 48 they differ by an inversion *and* a shift, so a driver
     * that got the height wrong passes one and fails the other. */
    {
        GLuint tall = 0, tall_fbo = 0;
        glGenTextures(1, &tall);
        glBindTexture(GL_TEXTURE_2D, tall);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 32, 48, 0, GL_RGBA,
                     GL_UNSIGNED_BYTE, NULL);
        glGenFramebuffers(1, &tall_fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, tall_fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, tall, 0);
        int ok = glCheckFramebufferStatus(GL_FRAMEBUFFER) ==
                 GL_FRAMEBUFFER_COMPLETE;
        if (ok) {
            glViewport(0, 0, 32, 48);
            glPolygonStipple(rows);
            glEnable(GL_POLYGON_STIPPLE);
            glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            static unsigned char tall_pixels[32 * 48 * 4];
            memset(tall_pixels, 0xAB, sizeof(tall_pixels));
            glReadPixels(0, 0, 32, 48, GL_RGBA, GL_UNSIGNED_BYTE, tall_pixels);
            glDisable(GL_POLYGON_STIPPLE);
            ok = glGetError() == GL_NO_ERROR;
            for (unsigned y = 0; y < 48; ++y) {
                for (unsigned x = 0; x < 32; x += 8) {
                    const int lit = tall_pixels[(y * 32 + x) * 4] > 128;
                    if (lit != ((y % 2) == 0)) ok = 0;
                }
            }
            fprintf(stderr, "INFO rows48 col0=%u,%u,%u,%u\n",
                    tall_pixels[0], tall_pixels[32 * 4],
                    tall_pixels[64 * 4], tall_pixels[96 * 4]);
        }
        result("rows_height_48", ok);
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glViewport(0, 0, 32, 32);
    }

    /* And with stippling off, every pixel. Refusing to draw at all would
     * otherwise pass the two cases above in the "clear" half. */
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    memset(pixels, 0xAB, sizeof(pixels));
    glReadPixels(0, 0, 32, 32, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    int all_lit = glGetError() == GL_NO_ERROR;
    for (unsigned y = 0; y < 32 && all_lit; ++y)
        for (unsigned x = 0; x < 32; ++x)
            if (at(x, y) <= 128) { all_lit = 0; break; }
    result("disabled_draws_everything", all_lit);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
