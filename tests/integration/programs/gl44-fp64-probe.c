/* Double-precision floats in shaders -- `ARB_gpu_shader_fp64`, GL 4.0.
 *
 * WGSL has no `f64` at all, so every double operation has to become a pair of
 * 32-bit ones. Mesa already knows how: `nir_lower_doubles` against
 * `src/compiler/glsl/float64.glsl`, the same software fp64 Intel ships, plus
 * `nir_lower_int64` for the integer halves that lowering produces. The
 * driver's part is `lower_doubles_options` (already set) and `caps->doubles`.
 *
 * The catch, and the reason this probe exists rather than a one-line cap
 * flip: st only *builds* the support shader when
 * `ctx->Const.GLSLVersion >= 400`, and warns otherwise --
 *
 *     Mesa full software implementation of FP64 requires OpenGL >= 4.0
 *
 * -- after which `nir_lower_double_ops` hits `assert(softfp64 != NULL)` and
 * takes the process with it. `GLSLVersion` comes straight from
 * `caps->glsl_feature_level` and is *not* clamped to the GL version, so the
 * two have to move together: fp64 needs GLSL 400, and GLSL 400 is what makes
 * fp64 deliverable.
 *
 * The arithmetic is chosen so 32-bit floats cannot produce the answer:
 * 1.0 + 2^-30 is 1.0 in binary32 and distinguishable in binary64, so a
 * driver that quietly computed in single precision fails `precision` while
 * passing `add`.
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

static int has_extension(const char *want) {
    GLint n = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &n);
    if (glGetError() != GL_NO_ERROR) return 0;
    for (GLint i = 0; i < n; ++i) {
        const char *e = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (e && !strcmp(e, want)) return 1;
    }
    return 0;
}

static GLuint build(const char *fs_src) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1536] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO fs compile: %s\n", log);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1536] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return prog;
}

static unsigned run(GLuint prog) {
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    if (!prog) return 0;
    glUseProgram(prog);
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

    const int present = has_extension("GL_ARB_gpu_shader_fp64");
    const int attrib64 = has_extension("GL_ARB_vertex_attrib_64bit");
    const int gpu5 = has_extension("GL_ARB_gpu_shader5");
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO extension fp64=%d attrib64=%d gpu_shader5=%d "
            "version=%s glsl=%s\n", present, attrib64, gpu5,
            glGetString(GL_VERSION),
            glGetString(GL_SHADING_LANGUAGE_VERSION));
    result("extension_present", present);
    if (!present) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    GLuint colour = 0, fbo = 0;
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
    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* Ordinary double arithmetic. 0.25 + 0.5 = 0.75 -> 191. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_gpu_shader_fp64 : require\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  double a = 0.25lf;\n"
            "  double b = 0.5lf;\n"
            "  colour = vec4(float(a + b), 0, 0, 1);\n"
            "}\n");
        const unsigned got = run(prog);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO add got=%u want=191 err=0x%04x\n", got,
                (unsigned)error);
        result("add", prog && error == GL_NO_ERROR && got >= 189 && got <= 193);
    }

    /* Precision a binary32 cannot reach: 1.0 + 2^-30 rounds to 1.0 in single
     * precision, so a driver computing in floats sees no difference and
     * writes 0 where a real double writes 255. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_gpu_shader_fp64 : require\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  double one = 1.0lf;\n"
            "  double tiny = 1.0lf / 1073741824.0lf;\n"
            "  double sum = one + tiny;\n"
            "  colour = vec4(sum > one ? 1.0 : 0.0, 0, 0, 1);\n"
            "}\n");
        const unsigned got = run(prog);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO precision got=%u want=255 err=0x%04x\n", got,
                (unsigned)error);
        result("precision", prog && error == GL_NO_ERROR && got >= 250);
    }

    /* Multiply and divide, which the software path implements separately from
     * add -- nir_lower_ddiv and the mul in float64.glsl are different code. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_gpu_shader_fp64 : require\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  double a = 3.0lf;\n"
            "  double b = 8.0lf;\n"
            "  double q = a / b;\n"
            "  double m = q * 2.0lf;\n"
            "  colour = vec4(float(m), 0, 0, 1);\n"
            "}\n");
        const unsigned got = run(prog);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO muldiv got=%u want=191 err=0x%04x\n", got,
                (unsigned)error);
        result("muldiv",
               prog && error == GL_NO_ERROR && got >= 189 && got <= 193);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
