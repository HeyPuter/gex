/* GL compute shaders, dispatched and read back.
 *
 * `ARB_compute_shader` is advertised now: the adapter grants 1024 invocations
 * per workgroup once asked, `caps->compute` is set, and image load/store and
 * atomic counters -- the two extensions st_extensions gates compute behind --
 * are both on.
 *
 * **In GLSL 3.30, deliberately.** Mesa's rule is
 * `has_compute_shader() = ARB_compute_shader_enable || is_version(430, 310)`,
 * so a compute shader needs *either* GLSL 4.30 or the extension asked for by
 * name. The pre-existing `gl44-compute-probe` uses `#version 430` and so
 * cannot run below a GLSL 4.30 driver at all; this one asks for the extension
 * instead, which is what makes compute usable at the 3.30 this driver reports.
 *
 * The cases are ordered so a failure says which layer broke:
 *
 *   compile        the shader compiles and links at all
 *   dispatch       one workgroup writes what it was told to
 *   invocations    every invocation of a 64-wide group ran, exactly once
 *   two_groups     the grid dimension is honoured, not just the group
 *
 * `invocations` is the one worth having: a driver that dispatched a single
 * invocation, or the same one repeatedly, passes `dispatch` and fails here.
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

static GLuint build_compute(const char *src) {
    GLuint sh = glCreateShader(GL_COMPUTE_SHADER);
    if (!sh) return 0;
    glShaderSource(sh, 1, &src, NULL);
    glCompileShader(sh);
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[2048] = {0};
        glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compute compile: %s\n", log);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, sh);
    glLinkProgram(prog);
    glDeleteShader(sh);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[2048] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compute link: %s\n", log);
        return 0;
    }
    return prog;
}

#define GROUP 64
#define SLOTS 256

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

    GLint max_invocations = 0, max_shared = 0;
    glGetIntegerv(GL_MAX_COMPUTE_WORK_GROUP_INVOCATIONS, &max_invocations);
    glGetIntegerv(GL_MAX_COMPUTE_SHARED_MEMORY_SIZE, &max_shared);
    const int present = has_extension("GL_ARB_compute_shader");
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO extension present=%d max_invocations=%d shared=%d "
            "version=%s glsl=%s\n", present, max_invocations, max_shared,
            glGetString(GL_VERSION),
            glGetString(GL_SHADING_LANGUAGE_VERSION));
    result("extension_present", present && max_invocations >= 1024);
    if (!present) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    /* One invocation per slot, each writing its own index plus a constant, so
     * the readback distinguishes "every invocation ran" from "one did". */
    static const char *src =
        "#version 330 core\n"
        "#extension GL_ARB_compute_shader : require\n"
        "#extension GL_ARB_shader_storage_buffer_object : require\n"
        "layout(local_size_x = 64) in;\n"
        "layout(std430) buffer Out { uint value[]; };\n"
        "void main() {\n"
        "  uint i = gl_GlobalInvocationID.x;\n"
        "  value[i] = i + 1000u;\n"
        "}\n";
    GLuint prog = build_compute(src);
    result("compile", prog != 0);
    if (!prog) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    GLuint ssbo = 0;
    unsigned *zero = calloc(SLOTS, sizeof(unsigned));
    glGenBuffers(1, &ssbo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    glBufferData(GL_SHADER_STORAGE_BUFFER, SLOTS * sizeof(unsigned), zero,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
    glUseProgram(prog);

    unsigned *back = malloc(SLOTS * sizeof(unsigned));

    /* One workgroup: slots 0..63 written, 64.. untouched. */
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    memset(back, 0xAB, SLOTS * sizeof(unsigned));
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, SLOTS * sizeof(unsigned),
                       back);
    {
        const GLenum error = glGetError();
        fprintf(stderr, "INFO dispatch slot0=%u want=1000 slot63=%u want=1063 "
                "slot64=%u want=0 err=0x%04x\n", back[0], back[63], back[64],
                (unsigned)error);
        result("dispatch", error == GL_NO_ERROR && back[0] == 1000u);
        unsigned right = 0;
        for (unsigned i = 0; i < GROUP; ++i)
            if (back[i] == i + 1000u) ++right;
        fprintf(stderr, "INFO invocations correct=%u/%u beyond_group=%u\n",
                right, (unsigned)GROUP, back[GROUP]);
        result("invocations", right == GROUP && back[GROUP] == 0u);
    }

    /* Two workgroups, so the grid dimension is exercised rather than only the
     * local size. */
    glBufferData(GL_SHADER_STORAGE_BUFFER, SLOTS * sizeof(unsigned), zero,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
    glDispatchCompute(2, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    memset(back, 0xAB, SLOTS * sizeof(unsigned));
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, SLOTS * sizeof(unsigned),
                       back);
    {
        const GLenum error = glGetError();
        unsigned right = 0;
        for (unsigned i = 0; i < 2 * GROUP; ++i)
            if (back[i] == i + 1000u) ++right;
        fprintf(stderr, "INFO two_groups correct=%u/%u beyond=%u err=0x%04x\n",
                right, (unsigned)(2 * GROUP), back[2 * GROUP],
                (unsigned)error);
        result("two_groups", error == GL_NO_ERROR && right == 2 * GROUP &&
                             back[2 * GROUP] == 0u);
    }

    free(zero);
    free(back);
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
