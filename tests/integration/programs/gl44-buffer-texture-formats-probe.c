/* Buffer textures of formats narrower than 32 bits per component.
 *
 * A samplerBuffer is not a texture here at all: WebGPU has no texel buffer, so
 * the driver binds the buffer as a read-only storage buffer and lowers
 * texelFetch to word loads. That made the *word* the unit, and only the
 * 32-bit-per-component formats could be served -- while GL's table of required
 * buffer-texture formats has three widths, and the narrow ones are exactly
 * what an application reaches for when the data is a palette, an index list or
 * packed vertex attributes.
 *
 * Unpacking them is arithmetic on the containing word, and the cases below are
 * chosen so that each way of getting it wrong shows as a different number
 * rather than as nothing:
 *
 *   `r8ui`    four unsigned bytes in one word. A driver that read the whole
 *             word per texel returns the packed value instead of the byte, and
 *             one that got the shift backwards returns a different byte.
 *   `r8_unorm` the same bytes read as normalised floats, which is the only
 *             difference between R8UI and R8 -- 255 versus 1.0.
 *   `rgba8ui` four components inside one word, so the component offset has to
 *             be right as well as the texel's.
 *   `r16i`    signed 16-bit, with a negative value: sign extension is what
 *             separates -2 from 65534, and both are plausible.
 *   `r16f`    half floats, two to a word.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <math.h>
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

/* Each fragment reads texel `index` and writes it into the colour target,
 * scaled so an 8-bit target can carry it. */
static GLuint build(const char *sampler, const char *expr) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    char fs_src[600];
    snprintf(fs_src, sizeof(fs_src),
             "#version 330 core\n"
             "uniform %s src;\n"
             "uniform int index;\n"
             "out vec4 colour;\n"
             "void main() { colour = vec4(%s, 0.0, 0.0, 1.0); }\n",
             sampler, expr);
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
        fprintf(stderr, "INFO compile: %s\n", log);
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
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return prog;
}

/* Bind `data` as a buffer texture of `internal_format`, fetch texel `index`
 * through `expr`, and read the red channel back as an 8-bit value. */
static int fetch(const char *name, GLenum internal_format, const void *data,
                 unsigned bytes, const char *sampler, const char *expr,
                 int index) {
    GLuint buffer = 0, tex = 0;
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_TEXTURE_BUFFER, buffer);
    glBufferData(GL_TEXTURE_BUFFER, bytes, data, GL_STATIC_DRAW);
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_BUFFER, tex);
    glTexBuffer(GL_TEXTURE_BUFFER, internal_format, buffer);

    GLuint prog = build(sampler, expr);
    if (!prog) return -1;
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_BUFFER, tex);
    glUniform1i(glGetUniformLocation(prog, "src"), 0);
    glUniform1i(glGetUniformLocation(prog, "index"), index);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO %s got=%u err=0x%04x\n", name, px[0],
            (unsigned)error);
    glDeleteTextures(1, &tex);
    glDeleteBuffers(1, &buffer);
    glDeleteProgram(prog);
    return error == GL_NO_ERROR ? (int)px[0] : -1;
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

    GLuint colour = 0, fbo = 0, vao = 0;
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

    /* Texel 2 of four bytes, so both the word and the byte within it matter. */
    const unsigned char bytes[8] = {10, 20, 200, 40, 50, 60, 70, 80};
    int got = fetch("r8ui", GL_R8UI, bytes, sizeof(bytes), "usamplerBuffer",
                    "float(texelFetch(src, index).r) / 255.0", 2);
    result("r8ui", got >= 198 && got <= 202);

    got = fetch("r8_unorm", GL_R8, bytes, sizeof(bytes), "samplerBuffer",
                "texelFetch(src, index).r", 2);
    result("r8_unorm", got >= 198 && got <= 202);

    /* Texel 1 is bytes 4..7, whose green component is 60. */
    got = fetch("rgba8ui", GL_RGBA8UI, bytes, sizeof(bytes), "usamplerBuffer",
                "float(texelFetch(src, index).g) / 255.0", 1);
    result("rgba8ui", got >= 58 && got <= 62);

    /* -2 sign-extends to -2; read as unsigned it is 65534, and either way the
     * probe scales it so the two are far apart. */
    const short shorts[4] = {7, -2, 300, -1};
    got = fetch("r16i", GL_R16I, shorts, sizeof(shorts), "isamplerBuffer",
                "float(texelFetch(src, index).r + 130) / 255.0", 1);
    result("r16i", got >= 126 && got <= 130);

    /* 0.5 as a half float. */
    const unsigned short halves[4] = {0x3800, 0x3C00, 0x0000, 0x3400};
    got = fetch("r16f", GL_R16F, halves, sizeof(halves), "samplerBuffer",
                "texelFetch(src, index).r", 0);
    result("r16f", got >= 126 && got <= 130);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
