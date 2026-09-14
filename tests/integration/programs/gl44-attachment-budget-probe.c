/* How many colour attachments fit, and whether the driver's arithmetic for
 * that matches WebGPU's.
 *
 * WebGPU bounds the *combined bytes per sample* across colour attachments, and
 * the cost of a format is not its size: `rgba8unorm` costs **8** while
 * `r32uint` costs **4**, though both are four bytes wide. The driver charged
 * every four-byte format eight, so eight R32UI draw buffers looked like 64
 * against a budget of 32 and four of them were *dropped*.
 *
 * Dropping an attachment is silent. There is no error, no refusal the
 * application can see, and no mark on the picture: the texture simply is never
 * written, and whatever it held stays. KHR-GL45.texture_barrier is exactly
 * that shape -- eight R32UI attachments -- and the whole family failed with
 * half its textures untouched, which read as a barrier defect.
 *
 *   eight_r32ui   eight single-channel 32-bit targets. 8 x 4 = 32, which is
 *                 WebGPU's guaranteed minimum exactly, so every one of the
 *                 eight must be written. This is the case that was failing.
 *   four_rgba8    four four-channel byte targets. 4 x 8 = 32, also exactly the
 *                 minimum, and the case that must keep working -- an
 *                 accounting fix that charges rgba8 four instead of eight
 *                 would pass the case above and break this one, by building a
 *                 pipeline WebGPU rejects.
 *   eight_r8      eight one-byte targets, 8 x 1 = 8. Comfortably inside any
 *                 budget; here to separate "the budget is too small" from
 *                 "the accounting is wrong".
 *
 * Every case checks **all** of its attachments. Checking one would pass on a
 * driver that dropped the rest, which is the failure being measured.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

#define SIDE 8
#define MAX_TARGETS 8

static int cases;
static int passed;

static void result(const char *name, int ok, const char *detail)
{
    ++cases;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-12s %s\n", name, detail);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 400 core\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
    "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "}\n";

/* Each target gets a different value, so a target written by the *wrong*
 * shader output is as visible as one not written at all. */
static const char *fs_uint_src =
    "#version 400 core\n"
    "#define N 8\n"
    "out uvec4 colour[N];\n"
    "void main() {\n"
    "  for (int i = 0; i < N; ++i) colour[i] = uvec4(uint(i) + 1u, 0u, 0u, 1u);\n"
    "}\n";
static const char *fs_unorm_src =
    "#version 400 core\n"
    "#define N 4\n"
    "out vec4 colour[N];\n"
    "void main() {\n"
    "  for (int i = 0; i < N; ++i)\n"
    "    colour[i] = vec4(float(i * 16 + 16) / 255.0, 0.0, 0.0, 1.0);\n"
    "}\n";

static GLuint build(const char *fragment)
{
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fragment, NULL); glCompileShader(fs);
    GLuint p = glCreateProgram();
    glAttachShader(p, vs); glAttachShader(p, fs);
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(p, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return p;
}

/* One case: `count` targets of `internal`, drawn once, every target read back
 * and checked against what the shader wrote to it. */
static void run_case(const char *name, const char *fragment, unsigned count,
                     GLenum internal, GLenum read_format, GLenum read_type,
                     int integer)
{
    GLuint tex[MAX_TARGETS] = {0}, fbo = 0;
    GLenum buffers[MAX_TARGETS];
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glGenTextures((GLsizei)count, tex);
    for (unsigned i = 0; i < count; ++i) {
        glActiveTexture(GL_TEXTURE0 + i);
        glBindTexture(GL_TEXTURE_2D, tex[i]);
        glTexStorage2D(GL_TEXTURE_2D, 1, internal, SIDE, SIDE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glFramebufferTexture(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0 + i, tex[i], 0);
        buffers[i] = GL_COLOR_ATTACHMENT0 + i;
    }
    glDrawBuffers((GLsizei)count, buffers);
    const GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    GLuint prog = build(fragment);
    char detail[200];
    if (!prog || status != GL_FRAMEBUFFER_COMPLETE) {
        snprintf(detail, sizeof(detail), "status=0x%04x prog=%u", (unsigned)status,
                 prog);
        result(name, 0, detail);
        glDeleteProgram(prog);
        return;
    }
    glViewport(0, 0, SIDE, SIDE);
    glDisable(GL_DEPTH_TEST);
    glUseProgram(prog);
    /* Cleared to a value no target should end with, so "never written" and
     * "written with the right value" cannot be confused -- a zeroed texture
     * would otherwise match target 0 of an integer shader that writes i+1. */
    if (integer) {
        const GLuint clear[4] = {0xdeadu, 0, 0, 0};
        for (unsigned i = 0; i < count; ++i) glClearBufferuiv(GL_COLOR, (GLint)i, clear);
    } else {
        const GLfloat clear[4] = {0.75f, 0.0f, 0.0f, 1.0f};
        for (unsigned i = 0; i < count; ++i) glClearBufferfv(GL_COLOR, (GLint)i, clear);
    }
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glFinish();

    unsigned bad = 0;
    int first_bad = -1;
    unsigned got_first = 0, want_first = 0;
    for (unsigned i = 0; i < count; ++i) {
        glActiveTexture(GL_TEXTURE0 + i);
        glBindTexture(GL_TEXTURE_2D, tex[i]);
        unsigned got;
        if (integer) {
            GLuint image[SIDE * SIDE];
            glGetTexImage(GL_TEXTURE_2D, 0, read_format, read_type, image);
            got = image[0];
        } else {
            unsigned char image[SIDE * SIDE * 4];
            glGetTexImage(GL_TEXTURE_2D, 0, read_format, read_type, image);
            got = image[0];
        }
        const unsigned want = integer ? i + 1u : i * 16u + 16u;
        if (got != want) {
            if (!bad) { first_bad = (int)i; got_first = got; want_first = want; }
            ++bad;
        }
    }
    snprintf(detail, sizeof(detail),
             "%u targets, %u wrong, first %d got=%u want=%u err=0x%04x",
             count, bad, first_bad, got_first, want_first,
             (unsigned)glGetError());
    result(name, bad == 0, detail);
    glDeleteProgram(prog);
    glDeleteFramebuffers(1, &fbo);
    glDeleteTextures((GLsizei)count, tex);
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
                                EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE};
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, xa);
    if (x == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 2; }
    static const EGLint sa[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE};
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!eglMakeCurrent(d, s, s, x)) { printf("FATAL eglMakeCurrent\n"); return 2; }
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    GLint max_draw = 0;
    glGetIntegerv(GL_MAX_DRAW_BUFFERS, &max_draw);
    fprintf(stderr, "INFO GL_MAX_DRAW_BUFFERS=%d\n", max_draw);

    run_case("eight_r32ui", fs_uint_src, 8, GL_R32UI, GL_RED_INTEGER,
             GL_UNSIGNED_INT, 1);
    run_case("four_rgba8", fs_unorm_src, 4, GL_RGBA8, GL_RGBA,
             GL_UNSIGNED_BYTE, 0);
    run_case("eight_r8", fs_uint_src, 8, GL_R8UI, GL_RED_INTEGER,
             GL_UNSIGNED_INT, 1);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
