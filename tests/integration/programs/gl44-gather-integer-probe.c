/* textureGather on integer and depth textures.
 *
 * `textureGather` reads one component from each of the four texels a bilinear
 * filter would blend, and returns them in a fixed rotation. It is how a shader
 * implements its own filtering, a soft shadow, or a min/max over a
 * neighbourhood, and `KHR-GL44.texture_gather` is 75 cases about it.
 *
 * The float and unorm cases passed while **every** int, uint and depth case
 * failed -- in all three forms, plain, single-offset and four-offset -- with
 * no refusal and no validation error, so the driver believed it was working.
 * That split is the reason this probe exists separately from a float one: the
 * sampler a gather needs is not the sampler the texture wants. WebGPU forbids
 * a filtering sampler on an integer texture and on an unfiltered depth
 * texture, and requires a *comparison* sampler for a depth gather that
 * compares -- so three kinds of gather need three kinds of sampler, and
 * getting one wrong gives wrong values rather than an error.
 *
 *   `int_component0`  the four texels' red, in GL's order
 *   `int_component1`  the same texels' green, which is the component argument
 *                     doing its job rather than being ignored
 *   `uint_component0`  the unsigned form, a different WGSL texture type
 *   `depth_compare`   a gather through a shadow sampler: four comparison
 *                     results, two of which are on each side of the reference
 *
 * The four texels carry four distinct values, so a driver returning the same
 * texel four times, or the right four in the wrong rotation, fails rather
 * than coincidentally matching. GL's rotation is
 * (texel(i0,j1), texel(i1,j1), texel(i1,j0), texel(i0,j0)) -- anticlockwise
 * from the upper left, which is not the order anyone guesses.
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

#define SIDE 4

static GLuint build(const char *sampler, const char *body) {
    static const char *vs_src =
        "#version 400 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    char fs_src[600];
    snprintf(fs_src, sizeof(fs_src),
             "#version 400 core\n"
             "uniform %s src;\n"
             "out ivec4 colour;\n"
             "void main() { %s }\n", sampler, body);
    const char *fs_ptr = fs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_ptr, NULL);
    glCompileShader(fs);
    GLint ok = 0;
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO fs compile (%s): %s\n", sampler, log);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link (%s): %s\n", sampler, log);
        return 0;
    }
    return prog;
}

/* The same gather, done in the *vertex* shader and handed to the fragment
 * shader flat. KHR-GL44.texture_gather runs every case three times -- once
 * sampling in the vertex stage, once in the fragment stage, once in a compute
 * shader -- and they fail independently: an implicit-LOD decomposition is
 * legal in a fragment shader and rejected outside one, and a bind group is
 * built per stage, so a sample type or sampler kind that is wrong for one
 * stage can be right for another. A probe that only samples in the fragment
 * stage passes while two thirds of the suite fails. */
static GLuint build_vertex_sampled(const char *sampler, const char *body) {
    char vs_src[700];
    snprintf(vs_src, sizeof(vs_src),
             "#version 400 core\n"
             "uniform %s src;\n"
             "flat out ivec4 gathered;\n"
             "void main() {\n"
             "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
             "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
             "  gl_Position = vec4(p, 0.0, 1.0);\n"
             "  %s\n"
             "}\n", sampler, body);
    static const char *fs_src =
        "#version 400 core\n"
        "flat in ivec4 gathered;\n"
        "out ivec4 colour;\n"
        "void main() { colour = gathered; }\n";
    const char *vs_ptr = vs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_ptr, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(vs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO vs compile (%s): %s\n", sampler, log);
        return 0;
    }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link (%s): %s\n", sampler, log);
        return 0;
    }
    return prog;
}

/* Every case renders one quad into an integer attachment and reads the centre
 * pixel, so the four gathered values arrive as one RGBA texel. */
static void draw_and_read(GLuint prog, GLenum target, GLuint tex, GLint *out) {
    const GLint clear[4] = {-1, -1, -1, -1};
    glClearBufferiv(GL_COLOR, 0, clear);
    glUseProgram(prog);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(target, tex);
    glUniform1i(glGetUniformLocation(prog, "src"), 0);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA_INTEGER, GL_INT, out);
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32I, SIDE, SIDE, 0,
                 GL_RGBA_INTEGER, GL_INT, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "INFO no integer framebuffer\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 2;
    }
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* A 2x2 integer texture. Each texel's red differs, and its green differs
     * from its red, so the component argument is observable. */
    const GLint texels[4][4] = {
        {10, 11, 12, 13}, {20, 21, 22, 23},   /* row 0: (0,0) (1,0) */
        {30, 31, 32, 33}, {40, 41, 42, 43},   /* row 1: (0,1) (1,1) */
    };
    GLuint signed_tex = 0;
    glGenTextures(1, &signed_tex);
    glBindTexture(GL_TEXTURE_2D, signed_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32I, 2, 2, 0, GL_RGBA_INTEGER,
                 GL_INT, texels);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

    const GLuint utexels[4][4] = {
        {10, 11, 12, 13}, {20, 21, 22, 23},
        {30, 31, 32, 33}, {40, 41, 42, 43},
    };
    GLuint unsigned_tex = 0;
    glGenTextures(1, &unsigned_tex);
    glBindTexture(GL_TEXTURE_2D, unsigned_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32UI, 2, 2, 0, GL_RGBA_INTEGER,
                 GL_UNSIGNED_INT, utexels);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

    /* A 2x2 depth texture whose four values straddle the reference below. */
    const float depths[4] = {
        0.1f, 0.2f,   /* row 0 */
        0.3f, 0.4f,   /* row 1 */
    };
    GLuint depth_tex = 0;
    glGenTextures(1, &depth_tex);
    glBindTexture(GL_TEXTURE_2D, depth_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT32F, 2, 2, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, depths);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_MODE,
                    GL_COMPARE_REF_TO_TEXTURE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_FUNC, GL_LEQUAL);

    if (glGetError() != GL_NO_ERROR) {
        fprintf(stderr, "INFO setup failed\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 2;
    }

    /* Gathering at the centre of a 2x2 texture puts the footprint exactly on
     * the four texels, so no wrap rule or rounding enters the answer. */
    struct { const char *name; const char *sampler; const char *body;
             GLuint tex; GLint want[4]; } tests[] = {
        /* result = (texel(0,1), texel(1,1), texel(1,0), texel(0,0)).r */
        {"int_component0", "isampler2D",
         "colour = textureGather(src, vec2(0.5), 0);", 0,
         {30, 40, 20, 10}},
        /* the same texels' green */
        {"int_component1", "isampler2D",
         "colour = textureGather(src, vec2(0.5), 1);", 0,
         {31, 41, 21, 11}},
        {"uint_component0", "usampler2D",
         "colour = ivec4(textureGather(src, vec2(0.5), 0));", 0,
         {30, 40, 20, 10}},
        /* 0.25 <= 0.3 and 0.4, not <= 0.1 or 0.2, so two ones and two zeroes
         * -- and in GL's rotation the ones come first. */
        {"depth_compare", "sampler2DShadow",
         "colour = ivec4(textureGather(src, vec2(0.5), 0.25) * 100.0);", 0,
         {100, 100, 0, 0}},
    };
    tests[0].tex = signed_tex;
    tests[1].tex = signed_tex;
    tests[2].tex = unsigned_tex;
    tests[3].tex = depth_tex;

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        GLuint prog = build(tests[i].sampler, tests[i].body);
        GLint got[4] = {-9, -9, -9, -9};
        if (prog)
            draw_and_read(prog, GL_TEXTURE_2D, tests[i].tex, got);
        const GLenum error = glGetError();
        const int ok = prog && error == GL_NO_ERROR &&
                       !memcmp(got, tests[i].want, sizeof(got));
        fprintf(stderr, "INFO %s got=%d,%d,%d,%d want=%d,%d,%d,%d err=0x%04x\n",
                tests[i].name, got[0], got[1], got[2], got[3],
                tests[i].want[0], tests[i].want[1], tests[i].want[2],
                tests[i].want[3], (unsigned)error);
        result(tests[i].name, ok);
    }

    /* The vertex stage, which the suite exercises first and which fails on
     * its own terms. */
    struct { const char *name; const char *sampler; const char *body;
             GLuint tex; GLint want[4]; } vertex_tests[] = {
        {"int_in_vertex", "isampler2D",
         "gathered = textureGather(src, vec2(0.5), 0);", 0,
         {30, 40, 20, 10}},
        {"depth_in_vertex", "sampler2DShadow",
         "gathered = ivec4(textureGather(src, vec2(0.5), 0.25) * 100.0);", 0,
         {100, 100, 0, 0}},
    };
    vertex_tests[0].tex = signed_tex;
    vertex_tests[1].tex = depth_tex;
    for (unsigned i = 0; i < sizeof(vertex_tests) / sizeof(vertex_tests[0]);
         ++i) {
        GLuint prog = build_vertex_sampled(vertex_tests[i].sampler,
                                           vertex_tests[i].body);
        GLint got[4] = {-9, -9, -9, -9};
        if (prog)
            draw_and_read(prog, GL_TEXTURE_2D, vertex_tests[i].tex, got);
        const GLenum error = glGetError();
        const int ok = prog && error == GL_NO_ERROR &&
                       !memcmp(got, vertex_tests[i].want, sizeof(got));
        fprintf(stderr, "INFO %s got=%d,%d,%d,%d want=%d,%d,%d,%d err=0x%04x\n",
                vertex_tests[i].name, got[0], got[1], got[2], got[3],
                vertex_tests[i].want[0], vertex_tests[i].want[1],
                vertex_tests[i].want[2], vertex_tests[i].want[3],
                (unsigned)error);
        result(vertex_tests[i].name, ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
