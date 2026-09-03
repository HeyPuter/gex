/* ARB_texture_mirror_clamp_to_edge -- one of GL 4.4's five remaining items.
 *
 * A wrap mode: the coordinate is *mirrored* about zero and then clamped to the
 * edge, so |u| is what gets sampled and anything past one gives the last
 * texel. It is what you want for a texture that is symmetric about its own
 * origin -- a radial gradient, a mirrored decal, a half-stored lookup table --
 * where GL_MIRRORED_REPEAT would tile it and GL_CLAMP_TO_EDGE would flatten
 * the negative side.
 *
 * WebGPU has three address modes and this is not one of them: clamp-to-edge,
 * repeat and mirror-repeat. But the mode *is* exactly "take the absolute value
 * and then clamp to edge", so it is expressible as one instruction in the
 * shader over a clamp-to-edge sampler -- which is why it is worth doing rather
 * than refusing.
 *
 *   `positive`     u = 0.625, inside the texture, where every mode agrees
 *   `mirrored`     u = -0.625, which must read the *same* texel as +0.625 --
 *                  the case that separates this mode from clamp-to-edge, and
 *                  the whole point of the extension
 *   `beyond`       u = 1.4, clamped to the last texel
 *   `beyond-neg`   u = -1.4, mirrored to 1.4 and then clamped
 *   `t-axis`       the same on t, because a driver that lowers one axis and
 *                  forgets the other passes four cases out of five
 *   `control`      the same -0.625 read through GL_CLAMP_TO_EDGE, which must
 *                  give a *different* texel -- so "the mode was ignored"
 *                  cannot pass as "the mode worked"
 *
 * Four texels, GL_NEAREST, no filtering to be off by one about.
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

static GLuint prog;
static GLuint tex_u, tex_v;

static unsigned sample_at(GLuint tex, float u, float v, GLenum wrap_s,
                          GLenum wrap_t)
{
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, (GLint)wrap_s);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, (GLint)wrap_t);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glClearColor(0, 1, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "tex"), 0);
    glUniform2f(glGetUniformLocation(prog, "at"), u, v);
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

    const int have = have_extension("GL_ARB_texture_mirror_clamp_to_edge");
    fprintf(stderr, "INFO version=%s mirror_clamp_to_edge=%d\n",
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

    /* Four texels across, then four down, with distinct reds. */
    const unsigned char row[4 * 4] = {
        0,0,0,255,   85,0,0,255,   170,0,0,255,   255,0,0,255,
    };
    glGenTextures(1, &tex_u);
    glBindTexture(GL_TEXTURE_2D, tex_u);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 4, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, row);
    glGenTextures(1, &tex_v);
    glBindTexture(GL_TEXTURE_2D, tex_v);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 4, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, row);

    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "uniform sampler2D tex;\n"
        "uniform vec2 at;\n"
        "out vec4 colour;\n"
        "void main() { colour = texture(tex, at); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) return 2;

    const GLenum mirror = GL_MIRROR_CLAMP_TO_EDGE;
    struct {
        const char *name;
        GLuint tex;
        float u, v;
        GLenum wrap_s, wrap_t;
        unsigned want;
    } tests[] = {
        {"positive",   0, 0.625f, 0.5f, mirror, mirror, 170},
        {"mirrored",   0, -0.625f, 0.5f, mirror, mirror, 170},
        {"beyond",     0, 1.4f, 0.5f, mirror, mirror, 255},
        {"beyond-neg", 0, -1.4f, 0.5f, mirror, mirror, 255},
        {"t-axis",     1, 0.5f, -0.625f, mirror, mirror, 170},
        /* The mode ignored would read the same texel as clamp-to-edge, so
         * this case has to give a *different* answer from `mirrored`. */
        {"control",    0, -0.625f, 0.5f, GL_CLAMP_TO_EDGE, GL_CLAMP_TO_EDGE, 0},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        unsigned got = 0xffff;
        if (have)
            got = sample_at(tests[t].tex ? tex_v : tex_u, tests[t].u,
                            tests[t].v, tests[t].wrap_s, tests[t].wrap_t);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO %s got=%u want=%u err=0x%04x\n", tests[t].name,
                got, tests[t].want, (unsigned)error);
        result(tests[t].name,
               have && error == GL_NO_ERROR && got == tests[t].want);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
