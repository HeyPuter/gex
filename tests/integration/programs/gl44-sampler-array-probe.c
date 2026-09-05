/* A sampler array indexed by a value the compiler cannot fold.
 *
 * `texture(samplers[i], c)` with a variable `i` is ordinary GLSL -- a texture
 * atlas, a material array, any shader that loops over its inputs -- and WGSL
 * has no sampler arrays in any form, so the index cannot reach the module.
 * This driver refused the shader, which failed the link.
 *
 * It can be unrolled instead: the array is a known, small number of units, so
 * the instruction becomes one copy per unit under a comparison against the
 * index. What this probe pins down is that the unrolling picks the *right*
 * unit, because picking the wrong one is not a crash -- every case still
 * samples something and returns a colour.
 *
 *   `loop_index`   four 1x1 textures of four different colours, summed in a
 *                  loop over the array. The sum is only right if each
 *                  iteration reached its own texture; any permutation that
 *                  repeats or skips one changes it.
 *   `varying_index` the index comes from a vertex attribute rather than a
 *                  loop counter, so nothing can be folded at compile time,
 *                  and two draws with different indices must return different
 *                  colours.
 *
 * The colours are powers of four so that no sum of the wrong subset can equal
 * the right one.
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
#define UNITS 4

static const char *vs_src =
    "#version 330 core\n"
    "layout(location = 0) in vec2 at;\n"
    "void main() { gl_Position = vec4(at, 0.0, 1.0); }\n";

/* Summed in a loop: the driver must reach a different unit each iteration. */
static const char *fs_loop =
    "#version 330 core\n"
    /* GLSL 1.30 forbids a non-constant sampler index outright; ARB_gpu_shader5
     * is what allows the dynamically uniform form, and is the spelling that
     * works in a 3.30 context. */
    "#extension GL_ARB_gpu_shader5 : require\n"
    "uniform sampler2D pool[4];\n"
    "out vec4 colour;\n"
    "void main() {\n"
    "  float total = 0.0;\n"
    "  for (int i = 0; i < 4; ++i)\n"
    "    total += texture(pool[i], vec2(0.5)).r;\n"
    "  colour = vec4(total, 0.0, 0.0, 1.0);\n"
    "}\n";

/* The index arrives as a uniform set between draws, so it cannot be folded. */
static const char *fs_pick =
    "#version 330 core\n"
    "#extension GL_ARB_gpu_shader5 : require\n"
    "uniform sampler2D pool[4];\n"
    "uniform int which;\n"
    "out vec4 colour;\n"
    "void main() { colour = texture(pool[which], vec2(0.5)); }\n";

static GLuint build(const char *fs_src) {
    struct { GLenum stage; const char *src; } parts[] = {
        {GL_VERTEX_SHADER, vs_src}, {GL_FRAGMENT_SHADER, fs_src},
    };
    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < 2; ++i) {
        GLuint sh = glCreateShader(parts[i].stage);
        glShaderSource(sh, 1, &parts[i].src, NULL);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO compile: %s\n", log);
            glDeleteShader(sh);
            glDeleteProgram(prog);
            return 0;
        }
        glAttachShader(prog, sh);
        glDeleteShader(sh);
    }
    glBindAttribLocation(prog, 0, "at");
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

static unsigned draw_and_read(GLuint prog) {
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    for (unsigned u = 0; u < UNITS; ++u) {
        char name[16];
        snprintf(name, sizeof(name), "pool[%u]", u);
        glUniform1i(glGetUniformLocation(prog, name), (GLint)u);
    }
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    return px[0];
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

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0, vao = 0, buffer = 0;
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

    const float quad[8] = {-1, -1,  1, -1,  -1, 1,  1, 1};
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 2 * sizeof(float), NULL);

    /* 1, 4, 16 and 64 of 255: no wrong subset sums to 85. */
    const unsigned char levels[UNITS] = {1, 4, 16, 64};
    GLuint pool[UNITS] = {0};
    glGenTextures(UNITS, pool);
    for (unsigned u = 0; u < UNITS; ++u) {
        const unsigned char texel[4] = {levels[u], 0, 0, 255};
        glActiveTexture(GL_TEXTURE0 + u);
        glBindTexture(GL_TEXTURE_2D, pool[u]);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                     GL_UNSIGNED_BYTE, texel);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    }

    GLuint loop_prog = build(fs_loop);
    unsigned got = loop_prog ? draw_and_read(loop_prog) : 0;
    GLenum error = glGetError();
    fprintf(stderr, "INFO loop_index got=%u want=85 err=0x%04x\n", got,
            (unsigned)error);
    result("loop_index",
           loop_prog && error == GL_NO_ERROR && got >= 83 && got <= 87);

    GLuint pick_prog = build(fs_pick);
    int ok = pick_prog != 0;
    for (unsigned u = 0; u < UNITS && ok; ++u) {
        glUseProgram(pick_prog);
        glUniform1i(glGetUniformLocation(pick_prog, "which"), (GLint)u);
        const unsigned one = draw_and_read(pick_prog);
        fprintf(stderr, "INFO varying_index which=%u got=%u want=%u\n", u, one,
                levels[u]);
        ok = one == levels[u];
    }
    error = glGetError();
    result("varying_index", ok && error == GL_NO_ERROR);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
