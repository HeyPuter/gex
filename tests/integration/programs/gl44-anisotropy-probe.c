/* EXT_texture_filter_anisotropic, and the ARB form GL 4.6 requires.
 *
 * The quality anisotropic filtering buys is implementation-defined, so this
 * does not try to measure it. What it measures is the thing that was actually
 * wrong: this driver *refused* any sampler with anisotropy above one, and a
 * refused sampler binds as NULL -- so an application that turns anisotropy on,
 * which most games do by default, lost its wrap modes and filters along with
 * it and drew the wrong thing entirely. SuperTuxKart is run in this tree with
 * `--anisotropic=0` for that reason.
 *
 *   `advertised`  the extension is claimed and GL_MAX_TEXTURE_MAX_ANISOTROPY
 *                 is at least 16, which is what st/mesa gates the ARB form on
 *   `accepted`    setting GL_TEXTURE_MAX_ANISOTROPY raises no error
 *   `wrap-kept`   with anisotropy on, GL_REPEAT still repeats -- this is the
 *                 case a refused sampler fails, because the default it falls
 *                 back to clamps
 *   `filter-kept` with anisotropy on, a LINEAR magnification filter still
 *                 blends two texels rather than snapping to one
 *
 * The last two sample a 2x1 texture through a repeating wrap at a coordinate
 * outside [0,1], so a sampler that clamped instead would read the wrong texel
 * and a sampler that snapped instead of blending would read a whole one.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

#ifndef GL_TEXTURE_MAX_ANISOTROPY
#define GL_TEXTURE_MAX_ANISOTROPY 0x84FE
#endif
#ifndef GL_MAX_TEXTURE_MAX_ANISOTROPY
#define GL_MAX_TEXTURE_MAX_ANISOTROPY 0x84FF
#endif

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const GLubyte *one = glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (one && strcmp((const char *)one, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 8

static GLuint build(const char *coord) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    char fs_src[400];
    snprintf(fs_src, sizeof(fs_src),
             "#version 330 core\n"
             "uniform sampler2D src;\n"
             "out vec4 colour;\n"
             "void main() { colour = texture(src, %s); }\n", coord);
    const char *fs_ptr = fs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_ptr, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    return ok ? prog : 0;
}

static unsigned draw_and_read(GLuint prog, GLuint tex) {
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    glUniform1i(glGetUniformLocation(prog, "src"), 0);
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

    const int have = have_extension("GL_EXT_texture_filter_anisotropic");
    GLfloat most = 0.0f;
    glGetFloatv(GL_MAX_TEXTURE_MAX_ANISOTROPY, &most);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s ext=%d max_anisotropy=%.1f\n",
            glGetString(GL_VERSION), have, most);
    result("advertised", have && most >= 16.0f);

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

    /* Two texels: 0 and 200. A repeating wrap at u = 1.25 lands in the first
     * one; a clamping fallback would land in the second. */
    const unsigned char texels[2][4] = {{0, 0, 0, 255}, {200, 0, 0, 255}};
    GLuint tex = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, texels);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_REPEAT);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_REPEAT);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameterf(GL_TEXTURE_2D, GL_TEXTURE_MAX_ANISOTROPY, 16.0f);
    const GLenum set_error = glGetError();
    fprintf(stderr, "INFO accepted err=0x%04x\n", (unsigned)set_error);
    result("accepted", set_error == GL_NO_ERROR);

    /* u = 1.25 with GL_REPEAT is u = 0.25, the centre of texel 0, so the
     * blend is entirely texel 0: black. Clamped it would be texel 1: 200. */
    GLuint wrap_prog = build("vec2(1.25, 0.5)");
    unsigned got = wrap_prog ? draw_and_read(wrap_prog, tex) : 255;
    GLenum error = glGetError();
    fprintf(stderr, "INFO wrap-kept got=%u want<=4 err=0x%04x\n", got,
            (unsigned)error);
    result("wrap-kept", wrap_prog && error == GL_NO_ERROR && got <= 4);

    /* Halfway between the two texel centres, so LINEAR blends them evenly:
     * 100. A sampler that snapped to one would read 0 or 200. */
    GLuint filter_prog = build("vec2(0.5, 0.5)");
    got = filter_prog ? draw_and_read(filter_prog, tex) : 0;
    error = glGetError();
    fprintf(stderr, "INFO filter-kept got=%u want~=100 err=0x%04x\n", got,
            (unsigned)error);
    result("filter-kept",
           filter_prog && error == GL_NO_ERROR && got >= 96 && got <= 104);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
