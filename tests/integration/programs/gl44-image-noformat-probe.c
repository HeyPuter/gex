/* An image uniform declared with no format qualifier.
 *
 * GLSL allows `writeonly uniform image2D img;` with no `layout(rgba8)` on it,
 * and GL then says the store converts to the *image unit's* format -- the one
 * glBindImageTexture named, not one the shader declared. WebGPU has no such
 * thing: a storage texture binding carries a concrete format, and a shader
 * that names none reaches SPIR-V with an Unknown format, which is a different
 * capability and not one WGSL has.
 *
 * This driver refused such a shader outright, which is not a small corner:
 * every one of KHR-GL45.shading_language_420pack's 329 cases writes its result
 * through exactly this declaration. The format is bound state, like a buffer
 * texture's texel width, so the shader gets the same treatment those do -- no
 * base module, one variant per set of resolved formats.
 *
 *   `rgba8`   the store lands in an rgba8 image and reads back as the value
 *             written, so a driver that dropped the dispatch reads zero
 *   `r32f`    the *same shader* against an r32f image unit, which is a
 *             different WebGPU storage format -- so a driver that resolved the
 *             format once and cached one module writes through the wrong one,
 *             and a driver that keyed its variant properly does not
 *
 * The second case is the whole point of the pair: one format proves the
 * refusal was lifted, two prove it was lifted *correctly*.
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

/* No format qualifier, and writeonly -- which is what GLSL requires of an
 * image that has none. */
static const char *cs_src =
    "#version 330 core\n"
    "#extension GL_ARB_compute_shader : require\n"
    "#extension GL_ARB_shader_image_load_store : require\n"
    "layout(local_size_x = 1, local_size_y = 1) in;\n"
    "writeonly uniform image2D uni_image;\n"
    "void main() {\n"
    "  imageStore(uni_image, ivec2(gl_GlobalInvocationID.xy),\n"
    "             vec4(0.75, 0.5, 0.25, 1.0));\n"
    "}\n";

static GLuint build(void) {
    GLuint sh = glCreateShader(GL_COMPUTE_SHADER);
    if (!sh) {
        fprintf(stderr, "INFO no compute stage (0x%04x)\n",
                (unsigned)glGetError());
        return 0;
    }
    glShaderSource(sh, 1, &cs_src, NULL);
    glCompileShader(sh);
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        glDeleteShader(sh);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, sh);
    glLinkProgram(prog);
    glDeleteShader(sh);
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

/* Dispatch the shader against a texture of `internal_format` and read the
 * first texel's red channel back as a float in [0,1]. */
static float run(GLuint prog, GLenum internal_format, GLenum read_format,
                 const char *what) {
    GLuint tex = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, internal_format, SIDE, SIDE);
    glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_WRITE_ONLY,
                       internal_format);
    glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "uni_image"), 0);
    glDispatchCompute(SIDE, SIDE, 1);
    glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);

    float got = -1.0f;
    if (read_format == GL_UNSIGNED_BYTE) {
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
