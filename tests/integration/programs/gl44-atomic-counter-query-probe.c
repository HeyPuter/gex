/* glGetActiveAtomicCounterBufferiv, and the one pname of it that is an array.
 *
 * Every other property this call returns is a single integer -- the binding,
 * the data size, the counter count, the per-stage referenced flags -- so the
 * transport declared it single-valued and sent exactly one word back. But
 * GL_ATOMIC_COUNTER_BUFFER_ACTIVE_ATOMIC_COUNTER_INDICES is one index per
 * active atomic counter in the buffer, and only the first of them ever
 * crossed. The rest of the caller's array kept whatever was already in it,
 * and no GL error was raised, so a program that reflects over its own atomic
 * counters -- which is what an engine's shader introspection does -- read one
 * real index and then garbage.
 *
 * The same shape as GL_COMPUTE_WORK_GROUP_SIZE being three integers rather
 * than one, and the last of that shape in the generated query surface: every
 * other one is single-valued for every pname GL defines for it.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* Three counters in one buffer, so the indices array has three entries and a
 * transport that sends one is visibly short. */
static const char *cs_src =
    "#version 430 core\n"
    "layout(local_size_x = 1) in;\n"
    "layout(binding = 0, offset = 0)  uniform atomic_uint first;\n"
    "layout(binding = 0, offset = 4)  uniform atomic_uint second;\n"
    "layout(binding = 0, offset = 8)  uniform atomic_uint third;\n"
    "void main() {\n"
    "  atomicCounterIncrement(first);\n"
    "  atomicCounterIncrement(second);\n"
    "  atomicCounterIncrement(third);\n"
    "}\n";

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

    GLuint cs = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(cs, 1, &cs_src, NULL);
    glCompileShader(cs);
    GLint ok = 0; glGetShaderiv(cs, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024] = {0}; glGetShaderInfoLog(cs, sizeof log, NULL, log);
               fprintf(stderr, "INFO compile failed: %s\n", log);
               printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, cs);
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0}; glGetProgramInfoLog(prog, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }

    GLint buffers = 0;
    glGetProgramiv(prog, GL_ACTIVE_ATOMIC_COUNTER_BUFFERS, &buffers);
    fprintf(stderr, "INFO active_buffers=%d want=1\n", buffers);
    result("active_buffers", buffers == 1);

    GLint counters = 0;
    glGetActiveAtomicCounterBufferiv(prog, 0,
        GL_ATOMIC_COUNTER_BUFFER_ACTIVE_ATOMIC_COUNTERS, &counters);
    fprintf(stderr, "INFO active_counters=%d want=3 err=0x%04x\n", counters,
            (unsigned)glGetError());
    result("active_counters", counters == 3);

    GLint size = 0;
    glGetActiveAtomicCounterBufferiv(prog, 0,
        GL_ATOMIC_COUNTER_BUFFER_DATA_SIZE, &size);
    fprintf(stderr, "INFO data_size=%d want=12\n", size);
    result("data_size", size == 12);

    /* The array. Prefilled with a value the driver cannot produce, so a slot
     * left untouched is visible rather than reading as a plausible index. */
    GLint indices[4] = { -7, -7, -7, -7 };
    glGetActiveAtomicCounterBufferiv(prog, 0,
        GL_ATOMIC_COUNTER_BUFFER_ACTIVE_ATOMIC_COUNTER_INDICES, indices);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO indices=%d,%d,%d spare=%d err=0x%04x\n",
            indices[0], indices[1], indices[2], indices[3], (unsigned)error);
    /* GL does not promise an order, only that the three are distinct, in
     * range, and that all three were written. */
    int written = 0, distinct = 1, in_range = 1;
    for (int i = 0; i < 3; ++i) {
        if (indices[i] != -7) ++written;
        if (indices[i] < 0) in_range = 0;
        for (int j = i + 1; j < 3; ++j)
            if (indices[i] == indices[j]) distinct = 0;
    }
    result("indices_all_written", error == GL_NO_ERROR && written == 3);
    result("indices_distinct_and_valid", distinct && in_range);
    /* And nothing past the three it was entitled to write. */
    result("indices_no_overrun", indices[3] == -7);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
