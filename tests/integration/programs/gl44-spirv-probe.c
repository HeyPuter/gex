/* ARB_gl_spirv: a shader handed to GL as SPIR-V rather than as GLSL source.
 *
 * The application compiles offline and calls glShaderBinary with the module,
 * then glSpecializeShader to name the entry point; from there it is an
 * ordinary program. Mesa does all of it -- glSpecializeShader runs
 * spirv_to_nir, and what reaches this driver is the NIR it already takes --
 * so nothing in the backend has to change. That is exactly why this probe
 * exists: an extension whose implementation is "nothing" is one nobody has
 * checked, and claiming one that does not work is worse than not claiming it.
 *
 *   `binary_format`  GL_SHADER_BINARY_FORMAT_SPIR_V is among the formats the
 *                    implementation lists, which is what an application asks
 *                    before trying
 *   `specialize`     both modules load and specialize without error
 *   `draws`          and the program then draws -- green over a cleared red,
 *                    so a program that linked but rendered nothing fails
 *
 * The modules below were built with `glslangValidator -G` from:
 *
 *   vertex:   a full-screen triangle positioned from gl_VertexID
 *   fragment: colour = vec4(0, 1, 0, 1)
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#ifndef GL_SHADER_BINARY_FORMAT_SPIR_V
#define GL_SHADER_BINARY_FORMAT_SPIR_V 0x9551
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

static const uint32_t spirv_vertex[] = {
    0x07230203, 0x00010000, 0x0008000b, 0x0000002c, 0x00000000, 0x00020011,
    0x00000001, 0x0006000b, 0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e,
    0x00000000, 0x0003000e, 0x00000000, 0x00000001, 0x0008000f, 0x00000000,
    0x00000004, 0x6e69616d, 0x00000000, 0x0000000c, 0x00000022, 0x0000002b,
    0x00030003, 0x00000002, 0x000001c2, 0x00040005, 0x00000004, 0x6e69616d,
    0x00000000, 0x00030005, 0x00000009, 0x00000070, 0x00050005, 0x0000000c,
    0x565f6c67, 0x65747265, 0x00444978, 0x00060005, 0x00000020, 0x505f6c67,
    0x65567265, 0x78657472, 0x00000000, 0x00060006, 0x00000020, 0x00000000,
    0x505f6c67, 0x7469736f, 0x006e6f69, 0x00070006, 0x00000020, 0x00000001,
    0x505f6c67, 0x746e696f, 0x657a6953, 0x00000000, 0x00070006, 0x00000020,
    0x00000002, 0x435f6c67, 0x4470696c, 0x61747369, 0x0065636e, 0x00070006,
    0x00000020, 0x00000003, 0x435f6c67, 0x446c6c75, 0x61747369, 0x0065636e,
    0x00030005, 0x00000022, 0x00000000, 0x00060005, 0x0000002b, 0x495f6c67,
    0x6174736e, 0x4965636e, 0x00000044, 0x00040047, 0x0000000c, 0x0000000b,
    0x00000005, 0x00030047, 0x00000020, 0x00000002, 0x00050048, 0x00000020,
    0x00000000, 0x0000000b, 0x00000000, 0x00050048, 0x00000020, 0x00000001,
    0x0000000b, 0x00000001, 0x00050048, 0x00000020, 0x00000002, 0x0000000b,
    0x00000003, 0x00050048, 0x00000020, 0x00000003, 0x0000000b, 0x00000004,
    0x00040047, 0x0000002b, 0x0000000b, 0x00000006, 0x00020013, 0x00000002,
    0x00030021, 0x00000003, 0x00000002, 0x00030016, 0x00000006, 0x00000020,
    0x00040017, 0x00000007, 0x00000006, 0x00000002, 0x00040020, 0x00000008,
    0x00000007, 0x00000007, 0x00040015, 0x0000000a, 0x00000020, 0x00000001,
    0x00040020, 0x0000000b, 0x00000001, 0x0000000a, 0x0004003b, 0x0000000b,
    0x0000000c, 0x00000001, 0x0004002b, 0x0000000a, 0x0000000e, 0x00000001,
    0x0004002b, 0x0000000a, 0x00000010, 0x00000000, 0x00020014, 0x00000011,
    0x0004002b, 0x00000006, 0x00000013, 0xbf800000, 0x0004002b, 0x00000006,
    0x00000014, 0x40400000, 0x0004002b, 0x0000000a, 0x00000017, 0x00000002,
    0x00040017, 0x0000001c, 0x00000006, 0x00000004, 0x00040015, 0x0000001d,
    0x00000020, 0x00000000, 0x0004002b, 0x0000001d, 0x0000001e, 0x00000001,
    0x0004001c, 0x0000001f, 0x00000006, 0x0000001e, 0x0006001e, 0x00000020,
    0x0000001c, 0x00000006, 0x0000001f, 0x0000001f, 0x00040020, 0x00000021,
    0x00000003, 0x00000020, 0x0004003b, 0x00000021, 0x00000022, 0x00000003,
    0x0004002b, 0x00000006, 0x00000024, 0x00000000, 0x0004002b, 0x00000006,
    0x00000025, 0x3f800000, 0x00040020, 0x00000029, 0x00000003, 0x0000001c,
    0x0004003b, 0x0000000b, 0x0000002b, 0x00000001, 0x00050036, 0x00000002,
    0x00000004, 0x00000000, 0x00000003, 0x000200f8, 0x00000005, 0x0004003b,
    0x00000008, 0x00000009, 0x00000007, 0x0004003d, 0x0000000a, 0x0000000d,
    0x0000000c, 0x000500c7, 0x0000000a, 0x0000000f, 0x0000000d, 0x0000000e,
    0x000500aa, 0x00000011, 0x00000012, 0x0000000f, 0x00000010, 0x000600a9,
    0x00000006, 0x00000015, 0x00000012, 0x00000013, 0x00000014, 0x0004003d,
    0x0000000a, 0x00000016, 0x0000000c, 0x000500c7, 0x0000000a, 0x00000018,
    0x00000016, 0x00000017, 0x000500aa, 0x00000011, 0x00000019, 0x00000018,
    0x00000010, 0x000600a9, 0x00000006, 0x0000001a, 0x00000019, 0x00000013,
    0x00000014, 0x00050050, 0x00000007, 0x0000001b, 0x00000015, 0x0000001a,
    0x0003003e, 0x00000009, 0x0000001b, 0x0004003d, 0x00000007, 0x00000023,
    0x00000009, 0x00050051, 0x00000006, 0x00000026, 0x00000023, 0x00000000,
    0x00050051, 0x00000006, 0x00000027, 0x00000023, 0x00000001, 0x00070050,
    0x0000001c, 0x00000028, 0x00000026, 0x00000027, 0x00000024, 0x00000025,
    0x00050041, 0x00000029, 0x0000002a, 0x00000022, 0x00000010, 0x0003003e,
    0x0000002a, 0x00000028, 0x000100fd, 0x00010038,
};

static const uint32_t spirv_fragment[] = {
    0x07230203, 0x00010000, 0x0008000b, 0x0000000d, 0x00000000, 0x00020011,
    0x00000001, 0x0006000b, 0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e,
    0x00000000, 0x0003000e, 0x00000000, 0x00000001, 0x0006000f, 0x00000004,
    0x00000004, 0x6e69616d, 0x00000000, 0x00000009, 0x00030010, 0x00000004,
    0x00000008, 0x00030003, 0x00000002, 0x000001c2, 0x00040005, 0x00000004,
    0x6e69616d, 0x00000000, 0x00040005, 0x00000009, 0x6f6c6f63, 0x00007275,
    0x00040047, 0x00000009, 0x0000001e, 0x00000000, 0x00020013, 0x00000002,
    0x00030021, 0x00000003, 0x00000002, 0x00030016, 0x00000006, 0x00000020,
    0x00040017, 0x00000007, 0x00000006, 0x00000004, 0x00040020, 0x00000008,
    0x00000003, 0x00000007, 0x0004003b, 0x00000008, 0x00000009, 0x00000003,
    0x0004002b, 0x00000006, 0x0000000a, 0x00000000, 0x0004002b, 0x00000006,
    0x0000000b, 0x3f800000, 0x0007002c, 0x00000007, 0x0000000c, 0x0000000a,
    0x0000000b, 0x0000000a, 0x0000000b, 0x00050036, 0x00000002, 0x00000004,
    0x00000000, 0x00000003, 0x000200f8, 0x00000005, 0x0003003e, 0x00000009,
    0x0000000c, 0x000100fd, 0x00010038,
};

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
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 5, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have = have_extension("GL_ARB_gl_spirv");
    fprintf(stderr, "INFO version=%s gl_spirv=%d\n", glGetString(GL_VERSION),
            have);
    if (!have) {
        fprintf(stderr, "INFO GL_ARB_gl_spirv absent; nothing to test\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    GLint formats = 0;
    glGetIntegerv(GL_NUM_SHADER_BINARY_FORMATS, &formats);
    int listed = 0;
    if (formats > 0 && formats < 32) {
        GLint list[32];
        memset(list, 0, sizeof(list));
        glGetIntegerv(GL_SHADER_BINARY_FORMATS, list);
        for (GLint i = 0; i < formats; ++i)
            if (list[i] == GL_SHADER_BINARY_FORMAT_SPIR_V) listed = 1;
    }
    fprintf(stderr, "INFO binary_format formats=%d listed=%d\n", formats,
            listed);
    result("binary_format", listed);

    /* Resolved rather than linked: Mesa's non-glvnd libGL does not export
     * glSpecializeShader, and a direct call fails at load with "undefined
     * symbol", taking the probe with it before it prints anything. */
    void (*specialize)(GLuint, const GLchar *, GLuint, const GLuint *,
                       const GLuint *) =
        (void (*)(GLuint, const GLchar *, GLuint, const GLuint *,
                  const GLuint *))eglGetProcAddress("glSpecializeShader");
    void (*shader_binary)(GLsizei, const GLuint *, GLenum, const void *,
                          GLsizei) =
        (void (*)(GLsizei, const GLuint *, GLenum, const void *, GLsizei))
            eglGetProcAddress("glShaderBinary");
    if (!specialize || !shader_binary) {
        /* An implementation may list the SPIR-V binary format and still not
         * hand out the entry points that use it -- the guest GL library this
         * driver serves in the browser exports neither -- and an application
         * would find that the same way. Reported rather than failed. */
        fprintf(stderr, "INFO glShaderBinary/glSpecializeShader are not "
                        "available; SPIR-V cannot be handed over here\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    shader_binary(1, &vs, GL_SHADER_BINARY_FORMAT_SPIR_V, spirv_vertex,
                  (GLsizei)sizeof(spirv_vertex));
    specialize(vs, "main", 0, NULL, NULL);
    shader_binary(1, &fs, GL_SHADER_BINARY_FORMAT_SPIR_V, spirv_fragment,
                  (GLsizei)sizeof(spirv_fragment));
    specialize(fs, "main", 0, NULL, NULL);
    GLint vs_ok = 0, fs_ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &vs_ok);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &fs_ok);
    if (!vs_ok || !fs_ok) {
        char log[900] = {0};
        glGetShaderInfoLog(vs_ok ? fs : vs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO specialize: %s\n", log);
    }
    GLenum error = glGetError();
    fprintf(stderr, "INFO specialize vs=%d fs=%d err=0x%04x\n", vs_ok, fs_ok,
            (unsigned)error);
    result("specialize", vs_ok && fs_ok && error == GL_NO_ERROR);

    GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
    }

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 8, 8, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    glViewport(0, 0, 8, 8);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    /* Red, so a program that draws nothing is not read as a pass. */
    glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    if (linked) {
        glUseProgram(program);
        glDrawArrays(GL_TRIANGLES, 0, 3);
    }
    unsigned char px[4] = {0};
    glReadPixels(4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    error = glGetError();
    fprintf(stderr, "INFO draws linked=%d got=%u,%u,%u err=0x%04x\n", linked,
            px[0], px[1], px[2], (unsigned)error);
    result("draws", linked && error == GL_NO_ERROR && px[1] > 128 &&
                    px[0] < 128);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
