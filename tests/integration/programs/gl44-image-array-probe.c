/* An array of image uniforms: `layout(rgba8) uniform image2D img[N];`
 *
 * WGSL has no arrays of textures or samplers, and Tint says so about the
 * variable rather than about anything the shader does:
 *
 *   arrays of textures or samplers are not supported in WGSL; can't translate
 *   variable or function parameter: %19 = OpVariable %18 UniformConstant
 *
 * The combined-sampler split had always turned `sampler2D tex[N]` into N
 * variables; nothing did the same for images, so an ordinary image array
 * failed to compile with a message naming an OpVariable number. The elements
 * are consecutive image units, so the split is the same shape.
 *
 *   `stores`   four images bound to four units, each written with its own
 *              value through a constant index. Reading all four back is what
 *              catches a split that collapsed them onto one unit -- which is
 *              the plausible way to get this wrong, and leaves three of the
 *              four textures untouched rather than failing.
 *   `loads`    the same four read back *through the array* in the shader and
 *              summed, so the element-to-unit mapping has to be right in the
 *              read direction too. The values are powers of four, so no wrong
 *              subset sums to the same total.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define UNITS 4

static const char *cs_store =
    "#version 430 core\n"
    "layout(local_size_x = 1) in;\n"
    /* An explicit binding: an array's elements take consecutive
     * units from it, and without one every element defaults to
     * unit zero -- which is a probe that measures nothing. */
    "layout(binding = 0, r32ui) uniform uimage2D pool[4];\n"
    "void main() {\n"
    "  imageStore(pool[0], ivec2(0), uvec4(1u));\n"
    "  imageStore(pool[1], ivec2(0), uvec4(4u));\n"
    "  imageStore(pool[2], ivec2(0), uvec4(16u));\n"
    "  imageStore(pool[3], ivec2(0), uvec4(64u));\n"
    "}\n";

/* Summed in an unrolled loop, so the read direction is exercised through the
 * array rather than through four separate names. */
static const char *cs_load =
    "#version 430 core\n"
    "layout(local_size_x = 1) in;\n"
    /* An explicit binding: an array's elements take consecutive
     * units from it, and without one every element defaults to
     * unit zero -- which is a probe that measures nothing. */
    "layout(binding = 0, r32ui) uniform uimage2D pool[4];\n"
    "layout(binding = 4, r32ui) uniform uimage2D total;\n"
    "void main() {\n"
    "  uint sum = 0u;\n"
    "  for (int i = 0; i < 4; ++i)\n"
    "    sum += imageLoad(pool[i], ivec2(0)).r;\n"
    "  imageStore(total, ivec2(0), uvec4(sum));\n"
    "}\n";

static GLuint build(const char *src) {
    GLuint sh = glCreateShader(GL_COMPUTE_SHADER);
    if (!sh) return 0;
    glShaderSource(sh, 1, &src, NULL);
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

static unsigned read_first(GLuint tex) {
    unsigned texels[1] = {0};
    glBindTexture(GL_TEXTURE_2D, tex);
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RED_INTEGER, GL_UNSIGNED_INT, texels);
    return texels[0];
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
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint pool[UNITS] = {0}, total = 0;
    glGenTextures(UNITS, pool);
    for (unsigned u = 0; u < UNITS; ++u) {
        glBindTexture(GL_TEXTURE_2D, pool[u]);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, 1, 1);
        glBindImageTexture(u, pool[u], 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32UI);
    }
    glGenTextures(1, &total);
    glBindTexture(GL_TEXTURE_2D, total);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, 1, 1);
    glBindImageTexture(UNITS, total, 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32UI);

    const unsigned want[UNITS] = {1, 4, 16, 64};
    GLuint store_prog = build(cs_store);
    int ok = store_prog != 0;
    if (ok) {
        glUseProgram(store_prog);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);
    }
    for (unsigned u = 0; u < UNITS && ok; ++u) {
        const unsigned got = read_first(pool[u]);
        fprintf(stderr, "INFO stores unit=%u got=%u want=%u\n", u, got,
                want[u]);
        ok = got == want[u];
    }
    GLenum error = glGetError();
    result("stores", ok && error == GL_NO_ERROR);

    GLuint load_prog = build(cs_load);
    unsigned sum = 0;
    if (load_prog) {
        glUseProgram(load_prog);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);
        sum = read_first(total);
    }
    error = glGetError();
    fprintf(stderr, "INFO loads sum=%u want=85 err=0x%04x\n", sum,
            (unsigned)error);
    result("loads", load_prog && error == GL_NO_ERROR && sum == 85);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
