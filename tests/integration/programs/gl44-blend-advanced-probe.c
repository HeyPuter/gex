/* KHR_blend_equation_advanced: the blend equation runs in the shader.
 *
 * Mesa's GLSL linker writes all fourteen equations itself once the driver
 * says it has framebuffer fetch, and asks for one thing back: the destination
 * texel. This driver supplies it from the same colour-attachment snapshot
 * that serves glLogicOp -- see gex_wgpu_lower_fb_fetch -- so what is checked
 * here is that the destination reaching the shader is the real one.
 *
 * Non-coherent, so glBlendBarrier separates the two draws. Each case clears
 * to a destination, draws a source over it, and checks the arithmetic:
 *
 *   multiply  Cs*Cd            0.8 * 0.5 = 0.4
 *   screen    Cs+Cd-Cs*Cd      0.8 + 0.5 - 0.4 = 0.9
 *   darken    min(Cs,Cd)       0.5
 *   lighten   max(Cs,Cd)       0.8
 *
 * All with alpha 1 on both sides, which is the case where the premultiplied
 * form the spec is written in reduces to the plain one.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef GL_MULTIPLY_KHR
#define GL_MULTIPLY_KHR 0x9294
#define GL_SCREEN_KHR   0x9295
#define GL_DARKEN_KHR   0x9297
#define GL_LIGHTEN_KHR  0x9298
#endif

/* Resolved rather than linked: glBlendBarrier is absent from Mesa's non-glvnd
 * libGL, so a probe that calls it directly dies at *load* with "undefined
 * symbol" before printing a thing. The guest's libGL has the same gap. */
static void (*blend_barrier)(void);

static int cases = 0, failures = 0;

#define CHECK(condition, message) do { \
    if (!(condition)) { \
        fprintf(stderr, "blend advanced failed: %s\n", message); return __LINE__; } \
} while (0)

static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "blend advanced failed: compile: %s\n", log);
        exit(2);
    }
    return shader;
}

static void expect(const char *what, unsigned got, unsigned want) {
    ++cases;
    const int slack = 3;
    if ((int)got - (int)want > slack || (int)want - (int)got > slack) {
        ++failures;
        fprintf(stderr, "FAIL %s: red=%u expected %u\n", what, got, want);
    } else {
        fprintf(stderr, "ok   %s: red=%u\n", what, got);
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    CHECK(eglChooseConfig(display, config_attributes, &config, 1, &config_count),
          "eglChooseConfig");
    CHECK(config_count >= 1 && config != NULL, "no pbuffer config");
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    CHECK(context != EGL_NO_CONTEXT, "eglCreateContext");
    CHECK(surface != EGL_NO_SURFACE, "eglCreatePbufferSurface");
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    GLuint rb = 0, fbo = 0;
    glGenRenderbuffers(1, &rb);
    glBindRenderbuffer(GL_RENDERBUFFER, rb);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 8, 8);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, rb);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "framebuffer completeness");
    glViewport(0, 0, 8, 8);

    static const char *vs_src =
        "#version 420 core\n"
        "const vec2 p[3] = vec2[3](vec2(-1.0,-1.0), vec2(3.0,-1.0),\n"
        "                          vec2(-1.0, 3.0));\n"
        "void main() { gl_Position = vec4(p[gl_VertexID], 0.0, 1.0); }\n";
    /* blend_support_all_equations, so one program serves every mode. */
    static const char *fs_src =
        "#version 420 core\n"
        "#extension GL_KHR_blend_equation_advanced : require\n"
        "layout(blend_support_all_equations) out;\n"
        "layout(location = 0) out vec4 colour;\n"
        "uniform vec4 src;\n"
        "void main() { colour = src; }\n";

    GLuint program = glCreateProgram();
    glAttachShader(program, compile(GL_VERTEX_SHADER, vs_src));
    glAttachShader(program, compile(GL_FRAGMENT_SHADER, fs_src));
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "blend advanced failed: link: %s\n", log);
        return 2;
    }
    glUseProgram(program);
    const GLint src_location = glGetUniformLocation(program, "src");
    CHECK(src_location >= 0, "no src uniform");

    blend_barrier = (void (*)(void))eglGetProcAddress("glBlendBarrierKHR");
    if (!blend_barrier)
        blend_barrier = (void (*)(void))eglGetProcAddress("glBlendBarrier");
    CHECK(blend_barrier != NULL, "no glBlendBarrier");

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    struct { const char *name; GLenum mode; unsigned want; } modes[] = {
        {"multiply", GL_MULTIPLY_KHR, 102},   /* 0.8 * 0.5      = 0.400 */
        {"screen",   GL_SCREEN_KHR,   230},   /* 0.8+0.5-0.4    = 0.900 */
        {"darken",   GL_DARKEN_KHR,   128},   /* min(0.8, 0.5)  = 0.500 */
        {"lighten",  GL_LIGHTEN_KHR,  204},   /* max(0.8, 0.5)  = 0.800 */
    };

    for (unsigned i = 0; i < sizeof(modes) / sizeof(modes[0]); ++i) {
        /* Destination first, with blending off so the clear lands as given. */
        glDisable(GL_BLEND);
        const GLfloat dst[] = {0.5f, 0.5f, 0.5f, 1.0f};
        glClearBufferfv(GL_COLOR, 0, dst);

        glEnable(GL_BLEND);
        glBlendEquation(modes[i].mode);
        glUniform4f(src_location, 0.8f, 0.8f, 0.8f, 1.0f);
        /* Non-coherent: the destination must be settled before it is read. */
        if (blend_barrier) blend_barrier();
        glDrawArrays(GL_TRIANGLES, 0, 3);
        if (blend_barrier) blend_barrier();

        unsigned char pixel[4] = {0, 0, 0, 0};
        glReadPixels(4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
        CHECK(glGetError() == GL_NO_ERROR, "draw/readback error");
        expect(modes[i].name, pixel[0], modes[i].want);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures,
           failures);
    return 0;
}
