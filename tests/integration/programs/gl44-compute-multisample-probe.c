/* A multisampled sampler in a *compute* shader.
 *
 * Two defects sat on top of each other here and both were silent, because a
 * bind group WebGPU refuses means the dispatch runs and writes **nothing** --
 * which reads back as whatever the buffer already held.
 *
 *   1. The compute bind-group layout never said `multisampled`. The
 *      collection computed it and its own comment said "the layout has to say
 *      so", but never recorded it, so the entry took the default of false and
 *      a real four-sample texture met a layout claiming one:
 *
 *        Sample count (4) of [Texture (RGBA8Sint)] doesn't match expectation
 *        (multisampled: 0)
 *
 *   2. With that fixed, the sample *type* was wrong for an integer
 *      multisample texture: the decision tested `multisampled` first and
 *      short-circuited every integer one to UnfilterableFloat.
 *
 *        The shader's texture sample type (Sint) isn't compatible with the
 *        layout's texture sample type (UnfilterableFloat)
 *
 * The two facts are independent -- an integer texture is unfilterable by its
 * own type -- and only the float case needs multisampling to say anything.
 *
 *   `ran`            a sentinel the shader writes unconditionally. This is
 *                    the case that catches the whole class: a refused bind
 *                    group leaves it at its pre-set value, and no GL error is
 *                    raised anywhere.
 *   `samples_float`  textureSamples() on a sampler2DMS.
 *   `samples_int`    textureSamples() on an isampler2DMS, which is the
 *                    combination both defects needed.
 *
 * texelFetch's *value* is deliberately not asserted: filling a multisample
 * texture needs a render pass, and the binding being valid at all is what
 * this is about.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SAMPLES 4

static const char *cs_src =
    "#version 450 core\n"
    "layout(local_size_x = 1) in;\n"
    "uniform sampler2DMS tex_f;\n"
    "uniform isampler2DMS tex_i;\n"
    "layout(std430, binding = 0) buffer Out { uint v[4]; };\n"
    "void main() {\n"
    "  v[0] = 7u;\n"
    "  v[1] = uint(textureSamples(tex_f));\n"
    "  v[2] = uint(textureSamples(tex_i));\n"
    /* Read one, so the binding is genuinely used rather than merely
       declared -- an unused sampler can be optimised out entirely. */
    "  v[3] = uint(texelFetch(tex_i, ivec2(0), 0).r) & 0xffu;\n"
    "}\n";

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (d == EGL_NO_DISPLAY || !eglInitialize(d, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                         EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE};
    EGLConfig cfg = NULL; EGLint n = 0;
    if (!eglChooseConfig(d, ca, &cfg, 1, &n) || n < 1) return 2;
    /* 4.5 for textureSamples(). */
    const EGLint cxa[] = {EGL_CONTEXT_MAJOR_VERSION, 4,
                          EGL_CONTEXT_MINOR_VERSION, 5, EGL_NONE};
    const EGLint sa[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext cx = eglCreateContext(d, cfg, EGL_NO_CONTEXT, cxa);
    EGLSurface sf = eglCreatePbufferSurface(d, cfg, sa);
    if (cx == EGL_NO_CONTEXT || sf == EGL_NO_SURFACE ||
        !eglMakeCurrent(d, sf, sf, cx)) return 2;
    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint cs = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(cs, 1, &cs_src, NULL);
    glCompileShader(cs);
    GLint ok = 0;
    glGetShaderiv(cs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(cs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, cs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    GLuint tf = 0, ti = 0;
    glGenTextures(1, &tf);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, tf);
    glTexImage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, SAMPLES, GL_RGBA8,
                            8, 4, GL_TRUE);
    glGenTextures(1, &ti);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ti);
    glTexImage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, SAMPLES, GL_RGBA8I,
                            8, 4, GL_TRUE);
    if (glGetError() != GL_NO_ERROR) {
        fprintf(stderr, "INFO multisample textures unavailable\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    /* Pre-set to a value the shader never writes, so "the dispatch wrote
       nothing" and "the dispatch wrote zero" are different answers. */
    GLuint seed[4] = {99, 99, 99, 99};
    GLuint ssbo = 0;
    glGenBuffers(1, &ssbo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof seed, seed, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);

    glUseProgram(prog);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, tf);
    glUniform1i(glGetUniformLocation(prog, "tex_f"), 0);
    glActiveTexture(GL_TEXTURE1);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ti);
    glUniform1i(glGetUniformLocation(prog, "tex_i"), 1);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    glFinish();

    GLuint got[4] = {0, 0, 0, 0};
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof got, got);
    fprintf(stderr, "INFO v=%u,%u,%u,%u err=0x%04x\n", got[0], got[1], got[2],
            got[3], (unsigned)glGetError());
    result("ran", got[0] == 7u);
    result("samples_float", got[1] == (GLuint)SAMPLES);
    result("samples_int", got[2] == (GLuint)SAMPLES);

    while (glGetError() != GL_NO_ERROR) {}
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
