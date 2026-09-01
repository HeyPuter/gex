/* ARB_vertex_type_2_10_10_10_rev: the eight packed vertex formats.
 *
 * WebGPU can fetch exactly one of them (Unorm10_10_10_2). The gex_wgpu driver
 * takes the other seven as a raw dword and unpacks them in the vertex shader,
 * so this exists to check that the unpack agrees with a real GL driver rather
 * than merely producing something. Each case draws one flat-shaded quad whose
 * colour *is* the fetched attribute, then reads the middle pixel back.
 *
 * The signed cases deliberately put a negative in the green channel: read as
 * unsigned, a negative 10-bit field becomes a large positive one and saturates
 * to 255 instead of clamping to 0, so a missing sign extension is the
 * difference between 0 and 255 rather than a rounding difference.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* GL_*_2_10_10_10_REV puts the first component in the low ten bits and the
 * two-bit alpha at the top. For GL_BGRA the component order is reversed, so
 * the low field is blue -- which is exactly the swizzle under test. */
static GLuint pack(int f0, int f1, int f2, int f3) {
    return ((GLuint)(f3 & 0x3) << 30) | ((GLuint)(f2 & 0x3ff) << 20) |
           ((GLuint)(f1 & 0x3ff) << 10) | (GLuint)(f0 & 0x3ff);
}

static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 a_position;\n"
    "layout(location = 1) in vec4 a_value;\n"
    "flat out vec4 v_value;\n"
    "void main() {\n"
    "    v_value = a_value;\n"
    "    gl_Position = vec4(a_position, 0.0, 1.0);\n"
    "}\n";

static const char *fragment_source =
    "#version 330 core\n"
    "flat in vec4 v_value;\n"
    "uniform float u_scale;\n"
    "out vec4 fragment;\n"
    "void main() {\n"
    "    fragment = clamp(v_value * u_scale, 0.0, 1.0);\n"
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

struct probe_case {
    const char *name;
    GLint size;        /* 4 or GL_BGRA */
    GLenum type;
    GLboolean normalized;
    GLuint packed;
    float scale;
    unsigned char expected[4];
};

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
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count) ||
        config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
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
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "link failed: %s\n", log);
        return 2;
    }
    glUseProgram(program);
    const GLint scale_location = glGetUniformLocation(program, "u_scale");

    GLuint texture = 0, framebuffer = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 8, 8, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 8, 8);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* One quad covering the whole target, as two triangles. */
    static const float positions[12] = {
        -1.0f, -1.0f,  1.0f, -1.0f,  -1.0f, 1.0f,
         1.0f, -1.0f,  1.0f,  1.0f,  -1.0f, 1.0f,
    };
    GLuint position_buffer = 0, value_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(positions), positions, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    glGenBuffers(1, &value_buffer);
    glEnableVertexAttribArray(1);

    const struct probe_case probe_cases[] = {
        /* Unsigned normalized: 1023 -> 1.0, 511 -> ~0.4995, alpha 3 -> 1.0.
         * This is the one WebGPU fetches natively. */
        {"unorm", 4, GL_UNSIGNED_INT_2_10_10_10_REV, GL_TRUE,
         0, 1.0f, {255, 0, 127, 255}},
        /* Signed normalized: 511 -> 1.0, -511 -> -1.0 (clamped to 0),
         * 255 -> ~0.499, alpha 1 -> 1.0. */
        {"snorm", 4, GL_INT_2_10_10_10_REV, GL_TRUE,
         0, 1.0f, {255, 0, 127, 255}},
        /* Unnormalized: the field values arrive as floats, and dividing by
         * 255 makes each one land on an exact byte. */
        {"uscaled", 4, GL_UNSIGNED_INT_2_10_10_10_REV, GL_FALSE,
         0, 1.0f / 255.0f, {200, 100, 50, 1}},
        {"sscaled", 4, GL_INT_2_10_10_10_REV, GL_FALSE,
         0, 1.0f / 255.0f, {200, 0, 50, 1}},
        /* GL_BGRA: same expected colour as the unorm case, but the fields are
         * stored blue-first, so a driver that ignores the swizzle returns
         * 127,0,255,255 instead. */
        {"bgra_unorm", GL_BGRA, GL_UNSIGNED_INT_2_10_10_10_REV, GL_TRUE,
         0, 1.0f, {255, 0, 127, 255}},
    };

    const GLuint packed_values[] = {
        pack(1023, 0, 511, 3),      /* unorm     r,g,b,a       */
        pack(511, -511, 255, 1),    /* snorm     r,g,b,a       */
        pack(200, 100, 50, 1),      /* uscaled   r,g,b,a       */
        pack(200, -100, 50, 1),     /* sscaled   r,g,b,a       */
        pack(511, 0, 1023, 3),      /* bgra      b,g,r,a       */
    };

    for (unsigned i = 0; i < sizeof(probe_cases) / sizeof(probe_cases[0]); ++i) {
        const struct probe_case *test = &probe_cases[i];
        GLuint attribute[6];
        for (unsigned v = 0; v < 6; ++v)
            attribute[v] = packed_values[i];
        glBindBuffer(GL_ARRAY_BUFFER, value_buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(attribute), attribute,
                     GL_STATIC_DRAW);
        glVertexAttribPointer(1, test->size, test->type, test->normalized, 0,
                              NULL);
        const GLenum setup_error = glGetError();

        glUniform1f(scale_location, test->scale);
        glClearColor(0.0f, 0.0f, 0.0f, 0.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 6);

        unsigned char pixel[4] = {0, 0, 0, 0};
        glReadPixels(4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
        const GLenum error = glGetError();

        /* Two counts of slack: the normalized cases divide by 1023 and 511,
         * which do not land on byte boundaries, and where a value falls on a
         * rounding tie is a property of the GPU. */
        int ok = setup_error == GL_NO_ERROR && error == GL_NO_ERROR;
        for (unsigned c = 0; c < 4; ++c) {
            const int delta = (int)pixel[c] - (int)test->expected[c];
            if (delta > 2 || delta < -2) ok = 0;
        }
        printf("INFO %s rgba=%u,%u,%u,%u expected=%u,%u,%u,%u error=0x%04x\n",
               test->name, pixel[0], pixel[1], pixel[2], pixel[3],
               test->expected[0], test->expected[1], test->expected[2],
               test->expected[3], (unsigned)error);
        result(test->name, ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
