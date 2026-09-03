/* Texture buffer objects -- `samplerBuffer` and `texelFetch`.
 *
 * These are **core GL 3.1**, not an extension, and this driver advertises GL
 * 3.3 -- so an application writing a `samplerBuffer` against the version it is
 * told it has is entitled to expect one. It does not get one: the shader fails
 * to *link*.
 *
 *     link FAILED: error: gex_wgpu invalid or duplicate texture binding
 *
 * llvmpipe returns the right texel. This is therefore a hole in a version
 * already claimed rather than a missing extension, which is why it is worth a
 * probe of its own. Buffer textures are how a renderer feeds skinning
 * matrices, per-instance transforms and particle state, so it is not an
 * obscure corner.
 *
 * Mesa's own `ver_3_1` gate does not list `ARB_texture_buffer_object`, which
 * is why the version was granted without it -- Mesa is lenient here and the
 * GL specification is not.
 *
 * WebGPU has no texel buffer at all, so the answer has to be a storage-buffer
 * read plus a format decode. The formats below are the 32-bit-per-channel
 * ones, which are a direct word read and a bitcast; the packed 8- and 16-bit
 * formats need unpacking and are deliberately not covered yet, so a partial
 * implementation shows up as a smaller pass count rather than as a wrong
 * texel.
 *
 * RGB32F is included because it is what `ARB_texture_buffer_object_rgb32`
 * -- one of the two extensions GL 4.0 still wants -- is about.
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

#define SIDE 8

static GLuint build(const char *sampler_type, const char *fetch)
{
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    char fs_src[512];
    snprintf(fs_src, sizeof(fs_src),
             "#version 330 core\n"
             "uniform %s buf;\n"
             "out vec4 colour;\n"
             "void main() { colour = %s; }\n", sampler_type, fetch);
    const char *fs_ptr = fs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_ptr, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
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
        char log[768] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
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

    GLint max_texels = 0;
    glGetIntegerv(GL_MAX_TEXTURE_BUFFER_SIZE, &max_texels);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO max_texture_buffer_size=%d version=%s\n", max_texels,
            glGetString(GL_VERSION));

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

    /* Texel 1 of each buffer carries 0.25, 0.5, 0.75 -- 64, 128, 191 as
     * bytes. Texel 0 is deliberately different so a driver that ignores the
     * index is caught. */
    struct {
        const char *name;
        GLenum internal_format;
        const char *sampler;
        const char *fetch;
        unsigned components;
    } tests[] = {
        {"rgba32f", GL_RGBA32F, "samplerBuffer",
         "texelFetch(buf, 1)", 4},
        {"rgb32f",  GL_RGB32F,  "samplerBuffer",
         "vec4(texelFetch(buf, 1).xyz, 1.0)", 3},
        {"r32f",    GL_R32F,    "samplerBuffer",
         "vec4(texelFetch(buf, 1).x, 0.5, 0.75, 1.0)", 1},
        {"r32i",    GL_R32I,    "isamplerBuffer",
         "vec4(float(texelFetch(buf, 1).x) / 255.0, 0.5, 0.75, 1.0)", 1},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        GLuint prog = build(tests[t].sampler, tests[t].fetch);
        GLuint tex = 0, bo = 0;
        const unsigned comps = tests[t].components;
        float floats[8] = {0};
        int ints[8] = {0};
        /* Texel 0 first, then texel 1 with the values checked below. */
        for (unsigned c = 0; c < comps; ++c) floats[c] = -1.0f;
        floats[comps + 0] = 0.25f;
        if (comps > 1) floats[comps + 1] = 0.5f;
        if (comps > 2) floats[comps + 2] = 0.75f;
        if (comps > 3) floats[comps + 3] = 1.0f;
        ints[0] = -1;
        ints[comps + 0] = 64;
        const int integer = tests[t].internal_format == GL_R32I;
        glGenBuffers(1, &bo);
        glBindBuffer(GL_TEXTURE_BUFFER, bo);
        glBufferData(GL_TEXTURE_BUFFER, (GLsizeiptr)(comps * 2 * 4),
                     integer ? (const void *)ints : (const void *)floats,
                     GL_STATIC_DRAW);
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_BUFFER, tex);
        glTexBuffer(GL_TEXTURE_BUFFER, tests[t].internal_format, bo);

        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_BUFFER, tex);
            glUniform1i(glGetUniformLocation(prog, "buf"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO %s got=%u,%u,%u want=64,128,191 err=0x%04x\n",
                tests[t].name, px[0], px[1], px[2], (unsigned)error);
        result(tests[t].name,
               prog && error == GL_NO_ERROR &&
               px[0] >= 62 && px[0] <= 66 &&
               px[1] >= 126 && px[1] <= 130 &&
               px[2] >= 189 && px[2] <= 193);
        glDeleteTextures(1, &tex);
        glDeleteBuffers(1, &bo);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
