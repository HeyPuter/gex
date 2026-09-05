/* Blending into a floating-point colour buffer, and saying so.
 *
 * WebGPU blends every renderable format except the pure-integer ones, and the
 * 32-bit float set only with the Float32Blendable feature -- which this device
 * grants and the driver asks for. GL exposes the same question through
 * glGetInternalformativ(GL_FRAMEBUFFER_BLEND), which st/mesa answers by asking
 * the driver whether the format accepts PIPE_BIND_BLENDABLE.
 *
 * That bind was unhandled: the constant appeared nowhere in the driver, so
 * every query carrying it was refused as "bad binds" and *every* format
 * reported NONE, however ordinary. Nothing in the CTS noticed, because the
 * families that blend do it into rgba8 through paths that never ask -- which
 * is exactly why this probe exists.
 *
 *   `reports_blend`  the four non-integer formats report FULL, as llvmpipe
 *                    does. Integer formats are left out on purpose: WebGPU
 *                    cannot blend them and this driver says so, while
 *                    llvmpipe answers FULL, so including them would make the
 *                    probe differ from its reference for being *more*
 *                    accurate.
 *   `blends_float`   and the arithmetic actually happens: 0.25 over 0.5 under
 *                    GL_ONE/GL_ONE is 0.75 in an RGBA32F attachment, a value
 *                    no 8-bit target could hold exactly.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <math.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 330 core\n"
    "const vec2 c[4] = vec2[4](vec2(-1,-1), vec2(1,-1), vec2(-1,1), vec2(1,1));\n"
    "void main() { gl_Position = vec4(c[gl_VertexID], 0.0, 1.0); }\n";
static const char *fs_src =
    "#version 330 core\n"
    "uniform vec4 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = tint; }\n";

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE,
    };
    EGLConfig config = NULL; EGLint config_count = 0;
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

    /* Mesa's non-glvnd libGL does not export this one; a direct call dies at
     * load with "undefined symbol" before anything prints. */
    void (*getifv)(GLenum, GLenum, GLenum, GLsizei, GLint *) =
        (void (*)(GLenum, GLenum, GLenum, GLsizei, GLint *))
            eglGetProcAddress("glGetInternalformativ");
    if (!getifv) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 0; }

    struct { const char *name; GLenum format; } blendable[] = {
        {"RGBA32F", GL_RGBA32F}, {"R32F", GL_R32F},
        {"RGBA16F", GL_RGBA16F}, {"RGBA8", GL_RGBA8},
    };
    int all_full = 1;
    for (unsigned i = 0; i < 4; ++i) {
        GLint blend = 0;
        getifv(GL_TEXTURE_2D, blendable[i].format, GL_FRAMEBUFFER_BLEND, 1,
               &blend);
        fprintf(stderr, "INFO %s blend=0x%04x\n", blendable[i].name,
                (unsigned)blend);
        if (blend != GL_FULL_SUPPORT) all_full = 0;
    }
    result("reports_blend", all_full);

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32F, 8, 8, 0, GL_RGBA, GL_FLOAT, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    const GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    glViewport(0, 0, 8, 8);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    GLuint program = glCreateProgram();
    struct { GLenum stage; const char *src; } parts[] = {
        {GL_VERTEX_SHADER, vs_src}, {GL_FRAGMENT_SHADER, fs_src},
    };
    for (unsigned i = 0; i < 2; ++i) {
        GLuint sh = glCreateShader(parts[i].stage);
        glShaderSource(sh, 1, &parts[i].src, NULL);
        glCompileShader(sh);
        glAttachShader(program, sh);
        glDeleteShader(sh);
    }
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);

    float got[4] = {0, 0, 0, 0};
    if (linked && status == GL_FRAMEBUFFER_COMPLETE) {
        const GLint tint = glGetUniformLocation(program, "tint");
        glUseProgram(program);
        glDisable(GL_BLEND);
        glClearColor(0, 0, 0, 0);
        glClear(GL_COLOR_BUFFER_BIT);
        glUniform4f(tint, 0.5f, 0.5f, 0.5f, 0.5f);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        /* 0.25 added on top, which only an additive blend produces. */
        glEnable(GL_BLEND);
        glBlendFunc(GL_ONE, GL_ONE);
        glUniform4f(tint, 0.25f, 0.25f, 0.25f, 0.25f);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glDisable(GL_BLEND);
        glReadPixels(4, 4, 1, 1, GL_RGBA, GL_FLOAT, got);
    }
    const GLenum error = glGetError();
    fprintf(stderr, "INFO blends_float status=0x%04x got=%.4f err=0x%04x\n",
            (unsigned)status, got[0], (unsigned)error);
    result("blends_float", linked && status == GL_FRAMEBUFFER_COMPLETE &&
                           error == GL_NO_ERROR && fabsf(got[0] - 0.75f) < 0.01f);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
