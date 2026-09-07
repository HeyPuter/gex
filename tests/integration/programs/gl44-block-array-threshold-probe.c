/* How many uniform blocks in an array can actually deliver their data.
 *
 * gl44-block-array-fourteen-probe found that fourteen blocks at bindings 2..15
 * link cleanly, report every binding correctly and draw without error while
 * delivering none of their contents. Linking and binding are therefore not
 * where this breaks, so the useful question is where the cliff is: the WebGPU
 * device offers 12 uniform buffers per shader stage and the driver advertises
 * 15 blocks by spilling the remainder into storage bindings, which predicts a
 * change in behaviour somewhere around twelve.
 *
 * Each block count gets its own program, and each reports how many of its 2N
 * vec4s arrived, so the output is a curve rather than a verdict: a clean run
 * is every count intact, and a partial count says which blocks were lost.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-34s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static char vs_buffer[16384];
static const char *build_vs(unsigned count) {
    char *w = vs_buffer;
    w += sprintf(w,
        "#version 420 core\n"
        "layout(std140, binding = 2) uniform GOKU {\n"
        "    vec4 gohan;\n"
        "    vec4 goten;\n"
        "} goku[%u];\n"
        "out float verdict;\n"
        "void main() {\n"
        "    int good = 0;\n", count);
    for (unsigned i = 0; i < count; ++i) {
        const int b2 = (i >> 2) & 1, b1 = (i >> 1) & 1, b0 = i & 1;
        w += sprintf(w,
            "    if (goku[%u].gohan == vec4(%d.0, %d.0, %d.0, 0.0)) ++good;\n"
            "    if (goku[%u].goten == vec4(%d.0, %d.0, %d.0, 1.0)) ++good;\n",
            i, b2, b1, b0, i, b2, b1, b0);
    }
    w += sprintf(w,
        "    verdict = float(good);\n"
        "    gl_Position = vec4((gl_VertexID == 1) ? 3.0 : -1.0,\n"
        "                       (gl_VertexID == 2) ? 3.0 : -1.0, 0.0, 1.0);\n"
        "}\n");
    return vs_buffer;
}

static const char *fs_src =
    "#version 420 core\n"
    "in float verdict;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(verdict / 255.0, 0.0, 0.0, 1.0); }\n";

static GLuint fs;

static int arrived_for(unsigned count) {
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    const char *src = build_vs(count);
    glShaderSource(vs, 1, &src, NULL); glCompileShader(vs);
    GLint ok = 0; glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) { fprintf(stderr, "INFO count=%u compile failed\n", count);
               return -2; }
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[512] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO count=%u link failed: %s\n", count,
                           log);
                   return -1; }

    GLuint ubo[16];
    glGenBuffers((GLsizei)count, ubo);
    for (unsigned i = 0; i < count; ++i) {
        const float b2 = (float)((i >> 2) & 1), b1 = (float)((i >> 1) & 1),
                    b0 = (float)(i & 1);
        const float block[8] = { b2, b1, b0, 0.0f, b2, b1, b0, 1.0f };
        glBindBuffer(GL_UNIFORM_BUFFER, ubo[i]);
        glBufferData(GL_UNIFORM_BUFFER, sizeof block, block, GL_STATIC_DRAW);
        glBindBufferRange(GL_UNIFORM_BUFFER, i + 2, ubo[i], 0, sizeof block);
    }
    glUseProgram(program);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    unsigned char pixel[4] = {0};
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    while (glGetError() != GL_NO_ERROR) { }
    glDeleteBuffers((GLsizei)count, ubo);
    glDeleteProgram(program); glDeleteShader(vs);
    return pixel[0];
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glViewport(0, 0, 16, 16);

    static const unsigned counts[] = { 1, 2, 4, 8, 10, 11, 12, 13, 14 };
    for (unsigned k = 0; k < sizeof counts / sizeof counts[0]; ++k) {
        const unsigned count = counts[k];
        char name[48];
        snprintf(name, sizeof name, "blocks_%02u_deliver_all", count);
        eq(name, arrived_for(count), 2 * count);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
