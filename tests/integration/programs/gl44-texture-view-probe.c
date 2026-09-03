/* ARB_texture_view -- one of GL 4.3's five remaining items.
 *
 * glTextureView makes a new texture object that aliases part of another's
 * storage: a range of mip levels, a range of array layers, a different target
 * over the same layers, or the same bits seen as a different format of the
 * same class. Nothing is copied. A streaming system uses it to hand a shader
 * only the resident mip levels of a texture it is still loading; a renderer
 * uses it to read the layers of an array as a cube map without reallocating.
 *
 * WebGPU has most of this and not all of it. A `GPUTextureView` takes a
 * `baseMipLevel`/`mipLevelCount`, a `baseArrayLayer`/`arrayLayerCount` and a
 * `dimension` that may differ from the texture's -- so the level, layer and
 * target cases map directly. What it does *not* have is general format
 * reinterpretation: a view's format must be the texture's, or its sRGB
 * counterpart, and nothing else. So the last case below is expected to be the
 * hard one, and it is in the probe precisely so that a partial
 * implementation shows up as a smaller pass count rather than as an
 * extension claimed and quietly broken.
 *
 *   `levels`      a view of levels 1..2 of a four-level texture: the view's
 *                 level 0 is the original's level 1
 *   `layers`      a view of layers 2..3 of an array: the view's layer 0 is
 *                 the original's layer 2
 *   `array-as-2d` a plain 2D view of one layer of an array
 *   `cube-as-2d-array`  the six faces of a cube map read as an array, which
 *                 is how a shader filters a cube map by hand
 *   `srgb`        an sRGB view of a linear texture, which WebGPU allows and
 *                 which is how GL_FRAMEBUFFER_SRGB is expressed
 *   `format-class` an RGBA8UI view of an RGBA8 texture -- same bits, same
 *                 class, different type -- which WebGPU has no form of
 *
 * Every case writes a distinct byte into each level and layer, so a view that
 * resolves to the wrong subresource reads back the wrong number rather than
 * something plausible.
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

static GLuint build(const char *sampler, const char *body)
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
             "void main() { %s }\n", sampler, body);
    const char *ptr = fs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &ptr, NULL);
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
    glDeleteShader(vs);
    glDeleteShader(fs);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

static GLuint colour_fbo;

static unsigned sample_through(GLuint prog, GLenum target, GLuint tex)
{
    glBindFramebuffer(GL_FRAMEBUFFER, colour_fbo);
    glViewport(0, 0, SIDE, SIDE);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    if (!prog) return 0xffff;
    glUseProgram(prog);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(target, tex);
    glTexParameteri(target, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(target, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
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

    const int have = have_extension("GL_ARB_texture_view");
    fprintf(stderr, "INFO version=%s texture_view=%d\n",
            glGetString(GL_VERSION), have);

    GLuint colour_tex = 0, vao = 0;
    glGenTextures(1, &colour_tex);
    glBindTexture(GL_TEXTURE_2D, colour_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &colour_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, colour_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour_tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    static const char *float_body =
        "colour = vec4(texture(tex, vec2(0.5)).r, 0.0, 0.0, 1.0);";
    static const char *array_body =
        "colour = vec4(texture(tex, vec3(0.5, 0.5, 0.0)).r, 0.0, 0.0, 1.0);";

    /* Case 1: a level range. Four levels of an 8x8 texture, each filled with
     * its own byte, so the view's level 0 names exactly one of them. */
    {
        unsigned got = 0xffff;
        GLuint base = 0, view = 0, prog = 0;
        if (have) {
            glGenTextures(1, &base);
            glBindTexture(GL_TEXTURE_2D, base);
            glTexStorage2D(GL_TEXTURE_2D, 4, GL_RGBA8, SIDE, SIDE);
            for (unsigned l = 0; l < 4; ++l) {
                const unsigned side = SIDE >> l;
                unsigned char bytes[SIDE * SIDE * 4];
                memset(bytes, 0x20 + 0x30 * l, sizeof(bytes));
                glTexSubImage2D(GL_TEXTURE_2D, l, 0, 0, side, side, GL_RGBA,
                                GL_UNSIGNED_BYTE, bytes);
            }
            glGenTextures(1, &view);
            glTextureView(view, GL_TEXTURE_2D, base, GL_RGBA8, 1, 2, 0, 1);
            prog = build("sampler2D", float_body);
            got = sample_through(prog, GL_TEXTURE_2D, view);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO levels got=%u want=80 err=0x%04x\n", got,
                (unsigned)error);
        result("levels", have && prog && error == GL_NO_ERROR && got == 0x50);
        if (prog) glDeleteProgram(prog);
        if (view) glDeleteTextures(1, &view);
        if (base) glDeleteTextures(1, &base);
    }

    /* Case 2: a layer range of an array. */
    {
        unsigned got = 0xffff;
        GLuint base = 0, view = 0, prog = 0;
        if (have) {
            glGenTextures(1, &base);
            glBindTexture(GL_TEXTURE_2D_ARRAY, base);
            glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_RGBA8, SIDE, SIDE, 4);
            for (unsigned l = 0; l < 4; ++l) {
                unsigned char bytes[SIDE * SIDE * 4];
                memset(bytes, 0x11 + 0x22 * l, sizeof(bytes));
                glTexSubImage3D(GL_TEXTURE_2D_ARRAY, 0, 0, 0, l, SIDE, SIDE, 1,
                                GL_RGBA, GL_UNSIGNED_BYTE, bytes);
            }
            glGenTextures(1, &view);
            glTextureView(view, GL_TEXTURE_2D_ARRAY, base, GL_RGBA8, 0, 1, 2,
                          2);
            prog = build("sampler2DArray", array_body);
            got = sample_through(prog, GL_TEXTURE_2D_ARRAY, view);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO layers got=%u want=85 err=0x%04x\n", got,
                (unsigned)error);
        result("layers", have && prog && error == GL_NO_ERROR && got == 0x55);
        if (prog) glDeleteProgram(prog);
        if (view) glDeleteTextures(1, &view);
        if (base) glDeleteTextures(1, &base);
    }

    /* Case 3: one layer of an array seen as a plain 2D texture. */
    {
        unsigned got = 0xffff;
        GLuint base = 0, view = 0, prog = 0;
        if (have) {
            glGenTextures(1, &base);
            glBindTexture(GL_TEXTURE_2D_ARRAY, base);
            glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_RGBA8, SIDE, SIDE, 4);
            for (unsigned l = 0; l < 4; ++l) {
                unsigned char bytes[SIDE * SIDE * 4];
                memset(bytes, 0x11 + 0x22 * l, sizeof(bytes));
                glTexSubImage3D(GL_TEXTURE_2D_ARRAY, 0, 0, 0, l, SIDE, SIDE, 1,
                                GL_RGBA, GL_UNSIGNED_BYTE, bytes);
            }
            glGenTextures(1, &view);
            glTextureView(view, GL_TEXTURE_2D, base, GL_RGBA8, 0, 1, 3, 1);
            prog = build("sampler2D", float_body);
            got = sample_through(prog, GL_TEXTURE_2D, view);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO array-as-2d got=%u want=119 err=0x%04x\n", got,
                (unsigned)error);
        result("array-as-2d",
               have && prog && error == GL_NO_ERROR && got == 0x77);
        if (prog) glDeleteProgram(prog);
        if (view) glDeleteTextures(1, &view);
        if (base) glDeleteTextures(1, &base);
    }

    /* Case 4: a cube map's six faces read as an array. */
    {
        unsigned got = 0xffff;
        GLuint base = 0, view = 0, prog = 0;
        if (have) {
            glGenTextures(1, &base);
            glBindTexture(GL_TEXTURE_CUBE_MAP, base);
            glTexStorage2D(GL_TEXTURE_CUBE_MAP, 1, GL_RGBA8, SIDE, SIDE);
            for (unsigned f = 0; f < 6; ++f) {
                unsigned char bytes[SIDE * SIDE * 4];
                memset(bytes, 0x10 + 0x10 * f, sizeof(bytes));
                /* Per face, with glTexSubImage2D. A cube map is not a layered
                 * target for upload purposes -- glTexSubImage3D on
                 * GL_TEXTURE_CUBE_MAP is GL_INVALID_ENUM, which read as a
                 * missing view feature until the error code was looked at. */
                glTexSubImage2D(GL_TEXTURE_CUBE_MAP_POSITIVE_X + f, 0, 0, 0,
                                SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, bytes);
            }
            glGenTextures(1, &view);
            glTextureView(view, GL_TEXTURE_2D_ARRAY, base, GL_RGBA8, 0, 1, 4,
                          2);
            prog = build("sampler2DArray", array_body);
            got = sample_through(prog, GL_TEXTURE_2D_ARRAY, view);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO cube-as-2d-array got=%u want=80 err=0x%04x\n",
                got, (unsigned)error);
        result("cube-as-2d-array",
               have && prog && error == GL_NO_ERROR && got == 0x50);
        if (prog) glDeleteProgram(prog);
        if (view) glDeleteTextures(1, &view);
        if (base) glDeleteTextures(1, &base);
    }

    /* Case 5: an sRGB view of a linear texture. WebGPU allows exactly this
     * reinterpretation and no other, so it is the one format case a driver on
     * that API can serve -- and the value differs from the linear read, which
     * is what proves the view took effect. */
    {
        unsigned got = 0xffff;
        GLuint base = 0, view = 0, prog = 0;
        if (have) {
            unsigned char bytes[SIDE * SIDE * 4];
            memset(bytes, 0x80, sizeof(bytes));
            glGenTextures(1, &base);
            glBindTexture(GL_TEXTURE_2D, base);
            glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, SIDE, SIDE);
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIDE, SIDE, GL_RGBA,
                            GL_UNSIGNED_BYTE, bytes);
            glGenTextures(1, &view);
            glTextureView(view, GL_TEXTURE_2D, base, GL_SRGB8_ALPHA8, 0, 1, 0,
                          1);
            prog = build("sampler2D", float_body);
            got = sample_through(prog, GL_TEXTURE_2D, view);
        }
        const GLenum error = glGetError();
        /* 0x80 decoded from sRGB is about 0.2158, which is 55 as a byte --
         * and unmistakably not 128. */
        fprintf(stderr, "INFO srgb got=%u want~=55 err=0x%04x\n", got,
                (unsigned)error);
        result("srgb", have && prog && error == GL_NO_ERROR &&
                       got >= 52 && got <= 58);
        if (prog) glDeleteProgram(prog);
        if (view) glDeleteTextures(1, &view);
        if (base) glDeleteTextures(1, &base);
    }

    /* Case 6: the same bits as a different type in the same class. WebGPU has
     * no form of this, so it is the case that says how complete the
     * implementation is. */
    {
        unsigned got = 0xffff;
        GLuint base = 0, view = 0, prog = 0;
        if (have) {
            unsigned char bytes[SIDE * SIDE * 4];
            memset(bytes, 0x2a, sizeof(bytes));
            glGenTextures(1, &base);
            glBindTexture(GL_TEXTURE_2D, base);
            glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, SIDE, SIDE);
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIDE, SIDE, GL_RGBA,
                            GL_UNSIGNED_BYTE, bytes);
            glGenTextures(1, &view);
            glTextureView(view, GL_TEXTURE_2D, base, GL_RGBA8UI, 0, 1, 0, 1);
            prog = build("usampler2D",
                         "colour = vec4(float(texture(tex, vec2(0.5)).r) "
                         "/ 255.0, 0.0, 0.0, 1.0);");
            got = sample_through(prog, GL_TEXTURE_2D, view);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO format-class got=%u want=42 err=0x%04x\n", got,
                (unsigned)error);
        result("format-class",
               have && prog && error == GL_NO_ERROR && got == 0x2a);
        if (prog) glDeleteProgram(prog);
        if (view) glDeleteTextures(1, &view);
        if (base) glDeleteTextures(1, &base);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
