/* ARB_vertex_type_10f_11f_11f_rev -- one of GL 4.4's five remaining items.
 *
 * A vertex attribute packed into a single 32-bit word as three *unsigned
 * floats*: eleven bits for x, eleven for y, ten for z, in that order from the
 * low bit. It exists because normals, tangents and light directions do not
 * need sign bits or 32-bit precision, and a mesh with three of them per vertex
 * saves 36 bytes a vertex by using it.
 *
 * WebGPU has no such vertex format. What it does have is a plain `uint32`
 * fetch, so the answer is the one this driver already uses for the
 * 10/10/10/2 family: fetch the raw dword and unpack it in the vertex shader,
 * as a per-layout variant. `nir_format_unpack_11f11f10f` is the arithmetic,
 * so the only new parts are the format's admission and its place in the
 * unpack table.
 *
 *   `packed`    the three components read back through a flat varying
 *   `two-of-three`  the same attribute declared `vec2` in the shader, which
 *                   is legal GL and which a driver that always writes four
 *                   components gets wrong
 *   `alongside` a packed attribute next to an ordinary float3 one, so a
 *               variant that rewrites the wrong element is caught
 *
 * The values are exact in both widths -- an 11-bit float has five exponent
 * bits and six of mantissa, a 10-bit float five and five -- so the readback is
 * an equality, not a tolerance on a conversion.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
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

static int have_extension(const char *want)
{
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 8

/* An unsigned float with `exp_bits` exponent bits and `mantissa_bits` of
 * mantissa, which is what 11f and 10f are. Only exactly representable values
 * are asked for below, so no rounding is needed. */
static unsigned pack_ufloat(double value, unsigned mantissa_bits)
{
    if (value == 0.0) return 0;
    int exponent = 0;
    while (value >= 2.0) { value /= 2.0; ++exponent; }
    while (value < 1.0)  { value *= 2.0; --exponent; }
    const unsigned biased = (unsigned)(exponent + 15);
    const double fraction = value - 1.0;
    const unsigned mantissa =
        (unsigned)(fraction * (double)(1u << mantissa_bits) + 0.5);
    return (biased << mantissa_bits) | mantissa;
}

static GLuint build(const char *decls, const char *body)
{
    char vs_src[900];
    snprintf(vs_src, sizeof(vs_src),
             "#version 330 core\n"
             "%s\n"
             "flat out vec3 value;\n"
             "void main() {\n"
             "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
             "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
             "  gl_Position = vec4(p, 0.0, 1.0);\n"
             "  %s\n"
             "}\n", decls, body);
    static const char *fs_src =
        "#version 330 core\n"
        "flat in vec3 value;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(value, 1.0); }\n";
    const char *vs_ptr = vs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_ptr, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(vs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO vs compile: %s\n", log);
        glDeleteShader(vs);
        return 0;
    }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) { glDeleteShader(vs); glDeleteShader(fs); return 0; }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glDeleteShader(vs);
    glDeleteShader(fs);
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

    const int have = have_extension("GL_ARB_vertex_type_10f_11f_11f_rev");
    fprintf(stderr, "INFO version=%s vertex_type_10f_11f_11f_rev=%d\n",
            glGetString(GL_VERSION), have);

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
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);

    /* x = 0.5 and y = 0.25 as 11-bit floats, z = 0.75 as a 10-bit one,
     * packed from the low bit upwards -- which is what the _REV suffix
     * means. All three are exact in their widths. */
    const uint32_t word = pack_ufloat(0.5, 6) |
                          (pack_ufloat(0.25, 6) << 11) |
                          (pack_ufloat(0.75, 5) << 22);
    fprintf(stderr, "INFO packed word=0x%08x\n", word);

    GLuint vbo = 0;
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    {
        uint32_t four[4] = {word, word, word, word};
        glBufferData(GL_ARRAY_BUFFER, sizeof(four), four, GL_STATIC_DRAW);
    }

    /* Case 1: the whole vec3. */
    {
        unsigned char px[4] = {0};
        GLuint prog = have
            ? build("in vec3 packed_attr;", "value = packed_attr;") : 0;
        if (prog) {
            glBindAttribLocation(prog, 0, "packed_attr");
            glLinkProgram(prog);
            glEnableVertexAttribArray(0);
            glVertexAttribPointer(0, 3, GL_UNSIGNED_INT_10F_11F_11F_REV,
                                  GL_FALSE, 4, NULL);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
            glDisableVertexAttribArray(0);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO packed got=%u,%u,%u want=128,64,191 err=0x%04x\n",
                px[0], px[1], px[2], (unsigned)error);
        result("packed", have && prog && error == GL_NO_ERROR &&
                         px[0] >= 126 && px[0] <= 130 &&
                         px[1] >= 62 && px[1] <= 66 &&
                         px[2] >= 189 && px[2] <= 193);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 2: the same attribute taken as a vec2. GL delivers the first two
     * components and the shader must see exactly those. */
    {
        unsigned char px[4] = {0};
        GLuint prog = have
            ? build("in vec2 packed_attr;",
                    "value = vec3(packed_attr, 0.0);") : 0;
        if (prog) {
            glBindAttribLocation(prog, 0, "packed_attr");
            glLinkProgram(prog);
            glEnableVertexAttribArray(0);
            glVertexAttribPointer(0, 3, GL_UNSIGNED_INT_10F_11F_11F_REV,
                                  GL_FALSE, 4, NULL);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
            glDisableVertexAttribArray(0);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO two-of-three got=%u,%u,%u want=128,64,0 "
                "err=0x%04x\n", px[0], px[1], px[2], (unsigned)error);
        result("two-of-three", have && prog && error == GL_NO_ERROR &&
                              px[0] >= 126 && px[0] <= 130 &&
                              px[1] >= 62 && px[1] <= 66 && px[2] <= 2);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 3: a packed attribute in element 1, with an ordinary float
     * attribute in element 0 -- so a variant that unpacks the wrong element
     * reads the plain one as a bit pattern and produces nonsense. */
    {
        unsigned char px[4] = {0};
        GLuint prog = have
            ? build("in float plain;\nin vec3 packed_attr;",
                    "value = vec3(plain, packed_attr.y, packed_attr.z);") : 0;
        GLuint plain_vbo = 0;
        if (prog) {
            const float plains[4] = {0.25f, 0.25f, 0.25f, 0.25f};
            glGenBuffers(1, &plain_vbo);
            glBindBuffer(GL_ARRAY_BUFFER, plain_vbo);
            glBufferData(GL_ARRAY_BUFFER, sizeof(plains), plains,
                         GL_STATIC_DRAW);
            glBindAttribLocation(prog, 0, "plain");
            glBindAttribLocation(prog, 1, "packed_attr");
            glLinkProgram(prog);
            glEnableVertexAttribArray(0);
            glVertexAttribPointer(0, 1, GL_FLOAT, GL_FALSE, 4, NULL);
            glBindBuffer(GL_ARRAY_BUFFER, vbo);
            glEnableVertexAttribArray(1);
            glVertexAttribPointer(1, 3, GL_UNSIGNED_INT_10F_11F_11F_REV,
                                  GL_FALSE, 4, NULL);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
            glDisableVertexAttribArray(0);
            glDisableVertexAttribArray(1);
            glDeleteBuffers(1, &plain_vbo);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO alongside got=%u,%u,%u want=64,64,191 "
                "err=0x%04x\n", px[0], px[1], px[2], (unsigned)error);
        result("alongside", have && prog && error == GL_NO_ERROR &&
                            px[0] >= 62 && px[0] <= 66 &&
                            px[1] >= 62 && px[1] <= 66 &&
                            px[2] >= 189 && px[2] <= 193);
        if (prog) glDeleteProgram(prog);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
