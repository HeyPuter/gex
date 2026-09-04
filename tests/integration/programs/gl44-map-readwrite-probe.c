/* glMapBufferRange with both READ and WRITE, over a buffer a shader wrote.
 *
 * Read a counter back, adjust it, carry on -- that is what a read-write map is
 * for, and it is ordinary in a renderer that keeps state in a buffer the GPU
 * also updates. This driver discarded every such edit.
 *
 * The mechanism is worth knowing before touching the map paths. A map that
 * *reads* a GPU-written buffer cannot be served from the driver's shadow copy,
 * because the shadow does not know what the GPU wrote -- so it is served from
 * a private block read back off the device, and the caller's edits land there
 * and nowhere else. Unmap then uploaded from the *shadow*, which still held
 * the copy taken when the map was made, before the caller touched anything.
 * Both conditions matched a read-write map and the wrong one won.
 *
 *   `sees_gpu_write`  the mapped pointer shows what the shader wrote, which is
 *                     the half that already worked and is the control here
 *   `edit_lands`      an edit made through the map survives the unmap
 *   `neighbour_kept`  a word the map did not cover still holds the shader's
 *                     value -- so a fix that uploads the whole buffer from a
 *                     stale shadow fails this even while passing the one above
 *   `second_read`     mapping again for reading shows the edited value, not
 *                     the pre-edit one a stale shadow would report
 *
 * Every word gets a distinct value, so a range written to the wrong offset is
 * identifiable rather than merely wrong.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define WORDS 64
/* The mapped window sits away from the start so a write at the wrong offset
 * lands somewhere observable. */
#define AT 16
#define SPAN 8

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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    /* A compute shader fills the buffer, so its contents are the GPU's and
     * the driver cannot serve a read from a CPU-side copy. */
    static const char *cs_src =
        "#version 430\n"
        "layout(local_size_x = 64) in;\n"
        "layout(std430, binding = 0) buffer Out { uint value[]; };\n"
        "void main() {\n"
        "  value[gl_LocalInvocationIndex] = 1000u + gl_LocalInvocationIndex;\n"
        "}\n";
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, &cs_src, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    GLuint buffer = 0;
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    {
        GLuint zero[WORDS];
        memset(zero, 0, sizeof(zero));
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(zero), zero,
                     GL_DYNAMIC_COPY);
    }
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buffer);
    glUseProgram(program);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    if (glGetError() != GL_NO_ERROR) {
        fprintf(stderr, "INFO the dispatch failed\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    /* Read *and* write the window, adding 1 to each word. */
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    GLuint *mapped = glMapBufferRange(
        GL_SHADER_STORAGE_BUFFER, AT * sizeof(GLuint), SPAN * sizeof(GLuint),
        GL_MAP_READ_BIT | GL_MAP_WRITE_BIT);
    int saw_gpu = mapped != NULL;
    if (mapped) {
        for (unsigned i = 0; i < SPAN; ++i) {
            if (mapped[i] != 1000u + AT + i) saw_gpu = 0;
            mapped[i] = mapped[i] + 1u;
        }
        fprintf(stderr, "INFO mapped[0]=%u want=%u\n", mapped[0] - 1u,
                1000u + AT);
        glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    } else {
        fprintf(stderr, "INFO glMapBufferRange returned NULL\n");
    }
    result("sees_gpu_write", saw_gpu);

    GLuint after[WORDS];
    memset(after, 0, sizeof(after));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(after), after);
    GLenum error = glGetError();
    fprintf(stderr, "INFO after[%d]=%u want=%u  after[0]=%u want=%u "
            "err=0x%04x\n", AT, after[AT], 1000u + AT + 1u, after[0], 1000u,
            (unsigned)error);
    int edited = error == GL_NO_ERROR;
    for (unsigned i = 0; i < SPAN; ++i)
        if (after[AT + i] != 1000u + AT + i + 1u) edited = 0;
    result("edit_lands", edited);

    /* Words outside the window keep what the shader put there. */
    int neighbours = error == GL_NO_ERROR;
    for (unsigned i = 0; i < WORDS; ++i) {
        if (i >= AT && i < AT + SPAN) continue;
        if (after[i] != 1000u + i) neighbours = 0;
    }
    result("neighbours_kept", neighbours);

    /* And a fresh read map reports the edited values. */
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    const GLuint *again = glMapBufferRange(
        GL_SHADER_STORAGE_BUFFER, AT * sizeof(GLuint), SPAN * sizeof(GLuint),
        GL_MAP_READ_BIT);
    int second = again != NULL;
    if (again) {
        for (unsigned i = 0; i < SPAN; ++i)
            if (again[i] != 1000u + AT + i + 1u) second = 0;
        fprintf(stderr, "INFO second_read[0]=%u want=%u\n", again[0],
                1000u + AT + 1u);
        glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    }
    result("second_read", second);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
