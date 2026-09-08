/* Sampling stencil: ARB_stencil_texturing and ARB_texture_stencil8.
 *
 * Two of the six extensions GL 4.3 still wants, and they come as a pair --
 * st/mesa lights both from one `is_format_supported` question about
 * X24S8_UINT or S8X24_UINT as a sampler view, so there is no way to have one
 * without the other and no reason to want to.
 *
 * What they are for is reading a stencil buffer as data. A deferred renderer
 * that marks materials, lights or decals in stencil and then wants that mark
 * inside a shader has no other way to get at it: the stencil test can only
 * discard, and glReadPixels is a stall. So the two cases below are the two
 * shapes that actually occur --
 *
 *   `stencil-aspect`  a DEPTH24_STENCIL8 texture rendered to as a depth-
 *                     stencil attachment, then sampled with
 *                     GL_DEPTH_STENCIL_TEXTURE_MODE = GL_STENCIL_INDEX;
 *   `stencil8`        a standalone GL_STENCIL_INDEX8 texture uploaded from
 *                     the CPU, which is what ARB_texture_stencil8 adds.
 *
 * -- plus two that guard the way a driver is likely to get it wrong:
 *
 *   `depth-aspect`    the same texture sampled as depth *after* the stencil
 *                     read, because a driver that builds one view per texture
 *                     and caches it will return stencil here;
 *   `stencil-render`  a GL_STENCIL_INDEX8 texture used as the stencil
 *                     attachment and then sampled, which is the deferred case
 *                     above and the one that needs the format to be both
 *                     renderable and samplable.
 *
 * Stencil is an *integer* aspect, so the sampler is a `usampler2D` and the
 * value is exact -- 0x40 reads back as 64 or the driver is wrong. There is no
 * filtering to be off by one about, which makes this a sharper probe than most
 * of the texture ones: GL_NEAREST is the only legal filter for a stencil
 * sample and any driver that averages is caught by the first case.
 *
 * WebGPU has the pieces. `stencil8` is a real texture format, a depth-stencil
 * texture view takes `aspect: "stencil-only"`, and such a view binds as an
 * unfilterable `texture_2d<u32>` -- so this is a wiring exercise rather than
 * an emulation one, which is what makes it worth doing before the harder 4.3
 * items.
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

/* The sampled value is written straight out as a byte, so the check below is
 * an equality on an integer rather than a tolerance on a colour. */
static GLuint build(const char *sampler_type, const char *body)
{
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    char fs_src[640];
    snprintf(fs_src, sizeof(fs_src),
             "#version 330 core\n"
             "uniform %s tex;\n"
             "out vec4 colour;\n"
             "void main() { %s }\n", sampler_type, body);
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

static int have_extension(const char *want)
{
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS,
                                                      (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

static int at_least_4_3(void)
{
    GLint major = 0, minor = 0;
    glGetIntegerv(GL_MAJOR_VERSION, &major);
    glGetIntegerv(GL_MINOR_VERSION, &minor);
    while (glGetError() != GL_NO_ERROR) {}
    return major > 4 || (major == 4 && minor >= 3);
}

static GLuint colour_tex, colour_fbo;

/* Draw a full-screen quad sampling `tex` through `prog` into the colour
 * target, and return the centre pixel's red channel. */
static unsigned sample_once(GLuint prog, GLuint tex)
{
    glBindFramebuffer(GL_FRAMEBUFFER, colour_fbo);
    glViewport(0, 0, SIDE, SIDE);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_STENCIL_TEST);
    if (!prog) return 0xffff;
    glUseProgram(prog);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    glUniform1i(glGetUniformLocation(prog, "tex"), 0);
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

    /* glGetString(GL_EXTENSIONS) is *removed* in a core profile and raises
     * GL_INVALID_ENUM, so it reads as "no extensions" on a driver that has
     * them -- llvmpipe reported 4.6 core and zero for both of these before the
     * indexed query went in. Both are also core from 4.3, so either answer
     * counts. */
    const int have_texturing = have_extension("GL_ARB_stencil_texturing") ||
                               at_least_4_3();
    const int have_stencil8 = have_extension("GL_ARB_texture_stencil8") ||
                              at_least_4_3();
    fprintf(stderr, "INFO version=%s stencil_texturing=%d texture_stencil8=%d\n",
            glGetString(GL_VERSION), have_texturing, have_stencil8);

    glGenTextures(1, &colour_tex);
    glBindTexture(GL_TEXTURE_2D, colour_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &colour_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, colour_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour_tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* A depth-stencil texture with 0x40 written into stencil everywhere and a
     * known depth, produced by rendering rather than by upload -- the case
     * that matters is reading back a stencil buffer the GPU wrote. */
    GLuint ds = 0, ds_fbo = 0;
    glGenTextures(1, &ds);
    glBindTexture(GL_TEXTURE_2D, ds);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH24_STENCIL8, SIDE, SIDE, 0,
                 GL_DEPTH_STENCIL, GL_UNSIGNED_INT_24_8, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &ds_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, ds_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT,
                           GL_TEXTURE_2D, ds, 0);
    glDrawBuffer(GL_NONE);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE) {
        glViewport(0, 0, SIDE, SIDE);
        glClearDepth(0.5);
        glClearStencil(0x40);
        glClear(GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, 0);

    /* Case 1: the stencil aspect of a depth-stencil texture. */
    {
        unsigned got = 0xffff;
        GLuint prog = 0;
        if (have_texturing) {
            glBindTexture(GL_TEXTURE_2D, ds);
            glTexParameteri(GL_TEXTURE_2D, GL_DEPTH_STENCIL_TEXTURE_MODE,
                            GL_STENCIL_INDEX);
            prog = build("usampler2D",
                         "colour = vec4(float(texture(tex, vec2(0.5)).x) "
                         "/ 255.0, 0.0, 0.0, 1.0);");
            got = sample_once(prog, ds);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO stencil-aspect got=%u want=64 err=0x%04x\n", got,
                (unsigned)error);
        result("stencil-aspect",
               have_texturing && prog && error == GL_NO_ERROR && got == 64);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 1b: the same thing on the *other* packed format.
     *
     * Gallium spells a stencil aspect as a separate stencil-only format and
     * there are four of them -- X24S8, S8X24, X32_S8X24 and plain S8, which
     * is exactly the set util_format_stencil_only can return. A driver that
     * lists three works here and not there, and this driver listed three:
     * depth24-stencil8 sampled and depth32float-stencil8 was refused
     * outright, with no CTS case anywhere reporting the difference. */
    {
        unsigned got = 0xffff;
        GLuint prog = 0;
        GLuint f32 = 0, f32_fbo = 0;
        glGenTextures(1, &f32);
        glBindTexture(GL_TEXTURE_2D, f32);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH32F_STENCIL8, SIDE, SIDE, 0,
                     GL_DEPTH_STENCIL, GL_FLOAT_32_UNSIGNED_INT_24_8_REV,
                     NULL);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glGenFramebuffers(1, &f32_fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, f32_fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT,
                               GL_TEXTURE_2D, f32, 0);
        glDrawBuffer(GL_NONE);
        const int complete =
            glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
        if (complete) {
            glViewport(0, 0, SIDE, SIDE);
            glClearDepth(0.5);
            glClearStencil(0x40);
            glClear(GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
        }
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        if (have_texturing && complete) {
            glBindTexture(GL_TEXTURE_2D, f32);
            glTexParameteri(GL_TEXTURE_2D, GL_DEPTH_STENCIL_TEXTURE_MODE,
                            GL_STENCIL_INDEX);
            prog = build("usampler2D",
                         "colour = vec4(float(texture(tex, vec2(0.5)).x) "
                         "/ 255.0, 0.0, 0.0, 1.0);");
            got = sample_once(prog, f32);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO stencil-aspect-f32 got=%u want=64 "
                "complete=%d err=0x%04x\n", got, complete,
                (unsigned)error);
        result("stencil-aspect-f32",
               have_texturing && complete && prog && error == GL_NO_ERROR &&
               got == 64);
        if (prog) glDeleteProgram(prog);
        glDeleteFramebuffers(1, &f32_fbo);
        glDeleteTextures(1, &f32);
    }

    /* Case 2: the depth aspect of the same texture, read after the stencil
     * one. A cached per-texture view returns stencil here. */
    {
        unsigned got = 0xffff;
        GLuint prog = 0;
        if (have_texturing) {
            glBindTexture(GL_TEXTURE_2D, ds);
            glTexParameteri(GL_TEXTURE_2D, GL_DEPTH_STENCIL_TEXTURE_MODE,
                            GL_DEPTH_COMPONENT);
            prog = build("sampler2D",
                         "colour = vec4(texture(tex, vec2(0.5)).x, 0.0, 0.0, "
                         "1.0);");
            got = sample_once(prog, ds);
        }
        const GLenum error = glGetError();
        /* 0.5 depth as a byte, with a wide tolerance because a depth24 value
         * scaled to 8 bits is not required to land on 128 exactly. */
        fprintf(stderr, "INFO depth-aspect got=%u want~=128 err=0x%04x\n", got,
                (unsigned)error);
        result("depth-aspect",
               have_texturing && prog && error == GL_NO_ERROR &&
               got >= 125 && got <= 131);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 3: a standalone GL_STENCIL_INDEX8 texture, uploaded. */
    {
        unsigned got = 0xffff;
        GLuint prog = 0, tex = 0;
        if (have_stencil8) {
            unsigned char bytes[SIDE * SIDE];
            memset(bytes, 0x5a, sizeof(bytes));
            glGenTextures(1, &tex);
            glBindTexture(GL_TEXTURE_2D, tex);
            glTexImage2D(GL_TEXTURE_2D, 0, GL_STENCIL_INDEX8, SIDE, SIDE, 0,
                         GL_STENCIL_INDEX, GL_UNSIGNED_BYTE, bytes);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            prog = build("usampler2D",
                         "colour = vec4(float(texture(tex, vec2(0.5)).x) "
                         "/ 255.0, 0.0, 0.0, 1.0);");
            got = sample_once(prog, tex);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO stencil8 got=%u want=90 err=0x%04x\n", got,
                (unsigned)error);
        result("stencil8",
               have_stencil8 && prog && error == GL_NO_ERROR && got == 0x5a);
        if (prog) glDeleteProgram(prog);
        if (tex) glDeleteTextures(1, &tex);
    }

    /* Case 4: a GL_STENCIL_INDEX8 texture rendered to and then sampled --
     * the deferred-renderer shape, which needs the format renderable too. */
    {
        unsigned got = 0xffff;
        GLuint prog = 0, tex = 0, fbo = 0;
        int complete = 0;
        if (have_stencil8) {
            glGenTextures(1, &tex);
            glBindTexture(GL_TEXTURE_2D, tex);
            glTexImage2D(GL_TEXTURE_2D, 0, GL_STENCIL_INDEX8, SIDE, SIDE, 0,
                         GL_STENCIL_INDEX, GL_UNSIGNED_BYTE, NULL);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glGenFramebuffers(1, &fbo);
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_STENCIL_ATTACHMENT,
                                   GL_TEXTURE_2D, tex, 0);
            glDrawBuffer(GL_NONE);
            complete = glCheckFramebufferStatus(GL_FRAMEBUFFER) ==
                       GL_FRAMEBUFFER_COMPLETE;
            if (complete) {
                glViewport(0, 0, SIDE, SIDE);
                glClearStencil(0x21);
                glClear(GL_STENCIL_BUFFER_BIT);
            }
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            prog = build("usampler2D",
                         "colour = vec4(float(texture(tex, vec2(0.5)).x) "
                         "/ 255.0, 0.0, 0.0, 1.0);");
            got = sample_once(prog, tex);
        }
        const GLenum error = glGetError();
        fprintf(stderr,
                "INFO stencil-render complete=%d got=%u want=33 err=0x%04x\n",
                complete, got, (unsigned)error);
        result("stencil-render",
               have_stencil8 && complete && prog && error == GL_NO_ERROR &&
               got == 0x21);
        if (prog) glDeleteProgram(prog);
        if (fbo) glDeleteFramebuffers(1, &fbo);
        if (tex) glDeleteTextures(1, &tex);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
