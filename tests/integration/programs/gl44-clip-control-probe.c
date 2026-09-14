/* `glClipControl(..., GL_ZERO_TO_ONE)` -- ARB_clip_control, core GL 4.5.
 *
 * GL's default clip space runs z from -1 to 1 and the viewport transform maps
 * it into the depth buffer's [0, 1]. `GL_ZERO_TO_ONE` says the shader already
 * writes z in [0, 1] and no such remap should happen. It is not an exotic
 * mode: it is half of **reverse-Z**, the standard way a modern renderer gets
 * usable depth precision at distance, and every engine that does it calls
 * this function.
 *
 * WebGPU's clip space is [0, 1] already, so this driver runs
 * `nir_lower_clip_halfz` on every vertex shader to convert GL's default. With
 * GL_ZERO_TO_ONE that conversion must **not** run, or the depth is
 * transformed twice: a shader writing z = 0.5 lands at 0.75 instead of 0.5.
 *
 * Gallium carries the mode as `rasterizer->clip_halfz`, and the driver
 * advertises `caps->clip_halfz` and *accepts* the state. Accepting a state and
 * ignoring it is the silent shape -- no error, no refusal, geometry simply at
 * the wrong depth, which reads as z-fighting or as things vanishing behind
 * each other.
 *
 *   default_mode   the GL default, NEGATIVE_ONE_TO_ONE. A control: clip z 0
 *                  must land at depth 0.5. If this fails the subject is the
 *                  depth pipeline, not clip control.
 *   zero_to_one    GL_ZERO_TO_ONE, clip z 0.5 -> depth 0.5. The double
 *                  transform lands it at 0.75, which is what this catches.
 *   zero_to_one_lo the same at z 0.25 -> 0.25, where the double transform
 *                  gives 0.625. Two points, because a single one can be
 *                  matched by a wrong-but-coincidental mapping.
 *   reverse_z      what a renderer actually does: GL_ZERO_TO_ONE, depth
 *                  cleared to 0 and GL_GREATER, so a nearer fragment has the
 *                  *larger* depth. Wrong clip handling either loses the draw
 *                  entirely or writes the wrong value.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

#define SIDE 8

static int cases;
static int passed;
static void (*clip_control)(GLenum, GLenum);

static void result(const char *name, int ok, const char *detail)
{
    ++cases;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-15s %s\n", name, detail);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* The clip-space z comes from a uniform so one program serves every case and
 * the only thing that changes between them is the GL state. */
static const char *vs_src =
    "#version 400 core\n"
    "uniform float clipz;\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
    "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
    "  gl_Position = vec4(p, clipz, 1.0);\n"
    "}\n";
static const char *fs_src =
    "#version 400 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(1.0); }\n";

static GLuint prog;
static GLint clipz_at;

static float depth_at_centre(void)
{
    float z = -1.0f;
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_DEPTH_COMPONENT, GL_FLOAT, &z);
    return z;
}

/* One case: set the mode, clear, draw at `clipz`, read the depth back. */
static void run_case(const char *name, GLenum depth_mode, float clipz,
                     float want, GLenum func, float clear_depth)
{
    clip_control(GL_LOWER_LEFT, depth_mode);
    glClearDepth(clear_depth);
    glClear(GL_DEPTH_BUFFER_BIT | GL_COLOR_BUFFER_BIT);
    glEnable(GL_DEPTH_TEST);
    glDepthFunc(func);
    glDepthMask(GL_TRUE);
    glUseProgram(prog);
    glUniform1f(clipz_at, clipz);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glFinish();
    const float got = depth_at_centre();
    const GLenum err = glGetError();
    char detail[160];
    snprintf(detail, sizeof(detail),
             "clipz=%.3f depth=%.4f want=%.4f err=0x%04x", (double)clipz,
             (double)got, (double)want, (unsigned)err);
    /* A generous window: the depth buffer is 24-bit here and the arithmetic
     * is exact, so anything inside 1/256 is the right answer and the wrong
     * one is a quarter of the range away. */
    result(name, err == GL_NO_ERROR && got > want - 0.004f &&
                 got < want + 0.004f, detail);
    while (glGetError() != GL_NO_ERROR) { }
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(d, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 2; }
    static const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE};
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    static const EGLint xa[] = {EGL_CONTEXT_MAJOR_VERSION, 4,
                                EGL_CONTEXT_MINOR_VERSION, 5, EGL_NONE};
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, xa);
    if (x == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 2; }
    static const EGLint sa[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE};
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!eglMakeCurrent(d, s, s, x)) { printf("FATAL eglMakeCurrent\n"); return 2; }
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    /* Through eglGetProcAddress. This non-glvnd libGL does not export
     * glClipControl at all -- linking it dies at load with "undefined
     * symbol" -- and a loader is how every real application reaches it. */
    clip_control = (void (*)(GLenum, GLenum))eglGetProcAddress("glClipControl");
    if (!clip_control) {
        fprintf(stderr, "INFO no glClipControl entry point\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    GLuint vao = 0, colour = 0, depth = 0, fbo = 0;
    glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, SIDE, SIDE);
    glGenTextures(1, &depth);
    glBindTexture(GL_TEXTURE_2D, depth);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_DEPTH_COMPONENT24, SIDE, SIDE);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, colour, 0);
    glFramebufferTexture(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, depth, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n"); return 2;
    }
    glViewport(0, 0, SIDE, SIDE);

    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs_src, NULL); glCompileShader(v);
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs_src, NULL); glCompileShader(f);
    prog = glCreateProgram();
    glAttachShader(prog, v); glAttachShader(prog, f);
    glLinkProgram(prog);
    GLint ok = 0; glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    clipz_at = glGetUniformLocation(prog, "clipz");

    /* GL's default: clip z 0 is the middle of the depth range. */
    run_case("default_mode", GL_NEGATIVE_ONE_TO_ONE, 0.0f, 0.5f, GL_ALWAYS,
             1.0f);
    /* ZERO_TO_ONE: the shader's z *is* the depth. A second transform would
     * put 0.5 at 0.75 and 0.25 at 0.625. */
    run_case("zero_to_one", GL_ZERO_TO_ONE, 0.5f, 0.5f, GL_ALWAYS, 1.0f);
    run_case("zero_to_one_lo", GL_ZERO_TO_ONE, 0.25f, 0.25f, GL_ALWAYS, 1.0f);
    /* Reverse-Z as a renderer writes it: cleared to 0, GL_GREATER, near
     * things at the far end of the range. */
    run_case("reverse_z", GL_ZERO_TO_ONE, 0.75f, 0.75f, GL_GREATER, 0.0f);

    /* Back to the default, so nothing after this inherits it. */
    clip_control(GL_LOWER_LEFT, GL_NEGATIVE_ONE_TO_ONE);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
