/* An image uniform with no format qualifier, in a *fragment* shader.
 *
 * The graphics-stage counterpart of gl44-image-noformat-probe, and the gap it
 * leaves. GLSL allows `writeonly uniform image2D img;` with no `layout(rgba8)`
 * on it, and GL says the store converts to the *image unit's* format -- the
 * one glBindImageTexture named, not one the shader declared. WebGPU has no
 * such thing: a storage texture binding carries a concrete format, and a
 * shader that names none reaches SPIR-V with an Unknown format, which is a
 * different capability and not one WGSL has.
 *
 * The driver resolves that from the bound image unit and keys a shader variant
 * on the answer -- but only for compute, which is where the need was first
 * found. A graphics stage recorded the declared format as PIPE_FORMAT_NONE and
 * carried it into the bind group layout, where it becomes an Undefined storage
 * format and the layout is rejected.
 *
 *   `rgba8`  the store lands in an rgba8 image and reads back as the value
 *            written, so a draw that never happened reads zero
 *   `r32f`   the *same shader* against an r32f image unit, which is the case
 *            that says the format came from the unit rather than from a guess
 *
 * Both write 0.75 into red. A single point covers the one texel this reads.
 *
 * **This probe is expected to DIFF, and the reference is the one that is
 * wrong.** llvmpipe passes `rgba8` and returns uninitialised memory for
 * `r32f` -- -2.5e38, which is what glTexStorage2D left there, so its store
 * went nowhere. radeonsi returns 0.75 for both, as this driver does, and that
 * is the tie-breaker: a format-less store to a single-channel float unit is a
 * bug in llvmpipe rather than an unreasonable thing for a probe to ask.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <math.h>
#include <stdio.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 4

static const char *vs_src =
    "#version 330 core\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";

/* No format qualifier, and writeonly -- which is what GLSL requires of an
 * image that has none. */
static const char *fs_src =
    "#version 330 core\n"
    "#extension GL_ARB_shader_image_load_store : require\n"
    "writeonly uniform image2D uni_image;\n"
    "out vec4 colour;\n"
    "void main() {\n"
    "  imageStore(uni_image, ivec2(0, 0), vec4(0.75, 0.5, 0.25, 1.0));\n"
    "  colour = vec4(1.0);\n"
    "}\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL);
    glCompileShader(sh);
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s compile: %s\n", what, log);
        glDeleteShader(sh);
        return 0;
    }
    return sh;
}

static GLuint build(void) {
    GLuint vs = compile(GL_VERTEX_SHADER, vs_src, "vs");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !fs) return 0;
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glDeleteShader(vs);
    glDeleteShader(fs);
    GLint ok = 0;
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

/* Draw one point with the shader bound against a texture of
 * `internal_format`, and read the first texel's red channel back. */
static float run(GLuint prog, GLenum internal_format, GLenum read_type,
                 const char *what) {
    GLuint tex = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, internal_format, SIDE, SIDE);
    glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_WRITE_ONLY,
                       internal_format);
    glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "uni_image"), 0);
    glDrawArrays(GL_POINTS, 0, 1);
    glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);

    float got = -1.0f;
    if (read_type == GL_UNSIGNED_BYTE) {
        unsigned char texels[SIDE * SIDE * 4] = {0};
        glGetTexImage(GL_TEXTURE_2D, 0, GL_RGBA, GL_UNSIGNED_BYTE, texels);
        got = texels[0] / 255.0f;
    } else {
        float texels[SIDE * SIDE * 4] = {0};
        glGetTexImage(GL_TEXTURE_2D, 0, GL_RGBA, GL_FLOAT, texels);
        got = texels[0];
    }
    const GLenum error = glGetError();
    fprintf(stderr, "INFO %s red=%.4f err=0x%04x\n", what, got,
            (unsigned)error);
    glDeleteTextures(1, &tex);
    return error == GL_NO_ERROR ? got : -1.0f;
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
    /* 4.2 for ARB_shader_image_load_store as core; the extension line above
     * keeps the shader itself legal at 330. */
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 2, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    GLuint prog = build();
    if (!prog) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    /* 0.75 stored into an 8-bit unorm comes back as 191/255. */
    const float rgba8 = run(prog, GL_RGBA8, GL_UNSIGNED_BYTE, "rgba8");
    result("rgba8", fabsf(rgba8 - 0.75f) <= 0.01f);

    /* The same shader, a different storage format. r32f keeps 0.75 exactly. */
    const float r32f = run(prog, GL_R32F, GL_FLOAT, "r32f");
    result("r32f", fabsf(r32f - 0.75f) <= 0.001f);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
