/* ARB_texture_query_lod: textureQueryLOD.
 *
 * WGSL has no textureQueryLod and SPIR-V's OpImageQueryLod is one of the few
 * opcodes Tint refuses outright, so the driver lowers the query to arithmetic
 * on the coordinate's screen-space derivatives. That makes this a probe about
 * a computation rather than about a binding, and the answer is known on paper:
 * a full-screen quad over an NxN viewport moves a 0..1 coordinate by 1/N per
 * pixel, which across a texture S texels wide is S/N texels per pixel, so the
 * level of detail is log2(S/N).
 *
 * Choosing the viewport therefore chooses the expected answer, and the cases
 * below pick viewports that separate a correct lowering from the plausible
 * wrong ones. A lowering that forgets to scale by the texture size returns
 * log2(1/N) and fails every case. One that returns the clamped level for both
 * components -- textureQueryLOD returns clamped in .x and unclamped in .y --
 * passes the minification cases and fails the magnification one, where the
 * two genuinely differ. One that clamps to the wrong end fails it too.
 *
 * The results are read from a float attachment rather than an 8-bit one: at
 * 8 bits a level is a handful of codes wide, and a probe that cannot see a
 * tenth of a level cannot tell a good approximation from a bad one.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
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
    "out vec2 uv;\n"
    "void main() {\n"
    "    uv = pos * 0.5 + 0.5;\n"
    "    gl_Position = vec4(pos, 0.0, 1.0);\n"
    "}\n";

/* .x is the level actually sampled, clamped to the mip chain; .y is the
 * unclamped value the hardware computed before that clamp. Both are written
 * so a lowering that conflates them is visible. */
static const char *fragment_source =
    "#version 330 core\n"
    "#extension GL_ARB_texture_query_lod : require\n"
    "uniform sampler2D tex;\n"
    "in vec2 uv;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    vec2 lod = textureQueryLOD(tex, uv);\n"
    "    fragColour = vec4(lod.x, lod.y, 0.0, 1.0);\n"
    "}\n";

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

/* The texture is 256 square, so its chain is 9 levels and the highest level
 * that can be sampled is 8. */
#define TEXTURE_SIZE 256
#define TOP_LEVEL 8.0f

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

    /* The extension has to be there before a shader may require it. */
    const char *extensions = (const char *)glGetString(GL_EXTENSIONS);
    int advertised = extensions &&
                     strstr(extensions, "GL_ARB_texture_query_lod") != NULL;
    if (!advertised) {
        GLint count = 0;
        glGetIntegerv(GL_NUM_EXTENSIONS, &count);
        for (GLint i = 0; i < count && !advertised; ++i) {
            const char *name = (const char *)glGetStringi(GL_EXTENSIONS,
                                                          (GLuint)i);
            if (name && strcmp(name, "GL_ARB_texture_query_lod") == 0)
                advertised = 1;
        }
    }
    /* glGetString(GL_EXTENSIONS) is not legal on a core profile and leaves
     * GL_INVALID_ENUM behind, which the per-case error check would otherwise
     * attribute to the first draw. */
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO advertised=%d\n", advertised);
    result("query_lod_advertised", advertised);
    if (!advertised) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) {
        result("query_lod_compiles", 0);
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
        char log[1024] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link failed: %s\n", log);
        result("query_lod_compiles", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
    result("query_lod_compiles", 1);

    /* A full mip chain, so query_levels has something to report. The content
     * is irrelevant: only the size reaches the answer. */
    unsigned char *texels = malloc(TEXTURE_SIZE * TEXTURE_SIZE * 4);
    if (!texels) return 2;
    memset(texels, 0x80, (size_t)TEXTURE_SIZE * TEXTURE_SIZE * 4);
    GLuint texture = 0;
    glGenTextures(1, &texture);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, TEXTURE_SIZE, TEXTURE_SIZE, 0,
                 GL_RGBA, GL_UNSIGNED_BYTE, texels);
    free(texels);
    glGenerateMipmap(GL_TEXTURE_2D);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER,
                    GL_LINEAR_MIPMAP_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    static const float positions[12] = {
        -1.0f, -1.0f,  1.0f, -1.0f,  -1.0f, 1.0f,
         1.0f, -1.0f,  1.0f,  1.0f,  -1.0f, 1.0f,
    };
    GLuint position_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(positions), positions, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "tex"), 0);

    struct {
        const char *name;
        int viewport;
        float want_clamped;
        float want_raw;
    } tests[] = {
        /* One texel per pixel: the base level, and the one case where an
         * implementation that ignores the texture size still happens to be
         * off by a full 8 levels rather than by a subtle amount. */
        {"lod_one_to_one", TEXTURE_SIZE, 0.0f, 0.0f},
        /* Minified four to one. */
        {"lod_minified", TEXTURE_SIZE / 4, 2.0f, 2.0f},
        /* Minified to a single pixel, landing exactly on the top level, so a
         * clamp that is off by one level is visible here. */
        {"lod_minified_to_top", 1, TOP_LEVEL, TOP_LEVEL},
        /* Magnified four to one. The clamped and raw values differ here and
         * nowhere else, which is the whole reason the query returns two. */
        {"lod_magnified", TEXTURE_SIZE * 4, 0.0f, -2.0f},
    };

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        const int side = tests[i].viewport;
        GLuint colour = 0, framebuffer = 0;
        glGenTextures(1, &colour);
        glBindTexture(GL_TEXTURE_2D, colour);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32F, side, side, 0, GL_RGBA,
                     GL_FLOAT, NULL);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glGenFramebuffers(1, &framebuffer);
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, colour, 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) !=
            GL_FRAMEBUFFER_COMPLETE) {
            fprintf(stderr, "INFO %s: framebuffer incomplete at %dx%d\n",
                    tests[i].name, side, side);
            result(tests[i].name, 0);
            continue;
        }
        glViewport(0, 0, side, side);
        glBindTexture(GL_TEXTURE_2D, texture);
        glClearColor(-99.0f, -99.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 6);

        float got[4] = {-99.0f, -99.0f, -99.0f, -99.0f};
        glReadPixels(side / 2, side / 2, 1, 1, GL_RGBA, GL_FLOAT, got);
        const GLenum error = glGetError();
        /* A tenth of a level. GL lets an implementation approximate the
         * scale factor within generous bounds, and the derivatives themselves
         * are only defined per quad, so an exact match is not owed; every
         * wrong answer these cases can produce is a whole level or more
         * away. */
        const float tolerance = 0.1f;
        int ok = error == GL_NO_ERROR &&
                 fabsf(got[0] - tests[i].want_clamped) <= tolerance &&
                 fabsf(got[1] - tests[i].want_raw) <= tolerance;
        fprintf(stderr,
                "INFO %s viewport=%d clamped=%.4f (want %.1f) raw=%.4f "
                "(want %.1f) err=0x%04x\n",
                tests[i].name, side, got[0], tests[i].want_clamped, got[1],
                tests[i].want_raw, (unsigned)error);
        result(tests[i].name, ok);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &colour);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
