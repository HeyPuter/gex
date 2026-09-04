/* `.length()` on an unsized shader-storage array.
 *
 * GLSL lets the last member of a storage block be an array with no declared
 * size, and `.length()` asks how many elements the *bound range* actually
 * holds. It is how a shader iterates a buffer whose contents the CPU sizes
 * per frame -- a visible-object list, a particle pool, anything GPU-driven --
 * and this driver refused it outright, by name, which fails the shader and so
 * the program.
 *
 * WGSL has the same thing as `arrayLength`, and the driver's flattened
 * storage block -- a struct whose one member is a runtime-sized array of
 * words -- is exactly the shape it wants. What did not work was the route:
 * the SPIR-V emitter borrowed from zink takes the length against zink's own
 * descriptor array, indexed by buffer number, which names a binding layout
 * this driver does not have.
 *
 *   `whole`     the length of a block bound with glBindBufferBase, which is
 *               the whole buffer
 *   `range`     the same buffer bound with glBindBufferRange over *half* of
 *               it: the length must halve, so a driver reporting the
 *               allocation rather than the binding fails here and only here
 *   `trailing`  a block with a fixed member ahead of the unsized array, whose
 *               length is (size - offset) / stride and not size / stride
 *   `loop`      length() driving a loop that writes every element, so a wrong
 *               length is visible as unwritten or out-of-range output rather
 *               than only as a wrong number
 *
 * The lengths differ between cases, so a driver returning any single constant
 * fails at least two of them.
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

static GLuint build_compute(const char *source) {
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return program;
}

#define ELEMENTS 64

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

    GLint alignment = 0;
    glGetIntegerv(GL_SHADER_STORAGE_BUFFER_OFFSET_ALIGNMENT, &alignment);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s ssbo_align=%d\n", glGetString(GL_VERSION),
            alignment);

    /* The buffer under test, and a separate one to report into -- so the
     * answer never comes out of the buffer whose length is the question. */
    GLuint data = 0, out = 0;
    glGenBuffers(1, &data);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, data);
    glBufferData(GL_SHADER_STORAGE_BUFFER, ELEMENTS * (GLsizeiptr)sizeof(GLuint),
                 NULL, GL_DYNAMIC_COPY);
    glGenBuffers(1, &out);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    {
        const GLuint filler[4] = {0xdeadu, 0xdeadu, 0xdeadu, 0xdeadu};
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(filler), filler,
                     GL_DYNAMIC_COPY);
    }
    if (glGetError() != GL_NO_ERROR) {
        fprintf(stderr, "INFO setup failed\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 2;
    }

    /* Case 1 and 2 share a program: only the binding differs. */
    GLuint report = build_compute(
        "#version 430\n"
        "layout(local_size_x = 1) in;\n"
        "layout(std430, binding = 0) buffer Data { uint value[]; };\n"
        "layout(std430, binding = 1) buffer Out { uint answer[]; };\n"
        "void main() { answer[0] = uint(value.length()); }\n");
    GLuint got[4];

    if (report) {
        glUseProgram(report);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, data);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, out);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    }
    memset(got, 0, sizeof(got));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
    GLenum error = glGetError();
    fprintf(stderr, "INFO whole got=%u want=%d err=0x%04x\n", got[0], ELEMENTS,
            (unsigned)error);
    result("whole", report && error == GL_NO_ERROR && got[0] == ELEMENTS);

    /* Half the buffer, from the start -- offset zero, so nothing here depends
     * on the storage-buffer offset alignment. */
    if (report) {
        glUseProgram(report);
        glBindBufferRange(GL_SHADER_STORAGE_BUFFER, 0, data, 0,
                          (ELEMENTS / 2) * (GLsizeiptr)sizeof(GLuint));
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, out);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    }
    memset(got, 0, sizeof(got));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
    error = glGetError();
    fprintf(stderr, "INFO range got=%u want=%d err=0x%04x\n", got[0],
            ELEMENTS / 2, (unsigned)error);
    result("range", report && error == GL_NO_ERROR && got[0] == ELEMENTS / 2);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, data);

    /* A fixed member ahead of the array eats one element's worth of the
     * range, so the length is one less than the whole. */
    GLuint trailing = build_compute(
        "#version 430\n"
        "layout(local_size_x = 1) in;\n"
        "layout(std430, binding = 0) buffer Data { uint header; uint value[]; };\n"
        "layout(std430, binding = 1) buffer Out { uint answer[]; };\n"
        "void main() { answer[1] = uint(value.length()); }\n");
    if (trailing) {
        glUseProgram(trailing);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, data);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, out);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    }
    memset(got, 0, sizeof(got));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
    error = glGetError();
    fprintf(stderr, "INFO trailing got=%u want=%d err=0x%04x\n", got[1],
            ELEMENTS - 1, (unsigned)error);
    result("trailing",
           trailing && error == GL_NO_ERROR && got[1] == ELEMENTS - 1);

    /* length() bounding a loop that writes every element. The last element
     * and the sum are both checked, so a length that is too small leaves the
     * tail untouched and one that is too large is a write past the end. */
    GLuint fill = build_compute(
        "#version 430\n"
        "layout(local_size_x = 1) in;\n"
        "layout(std430, binding = 0) buffer Data { uint value[]; };\n"
        "layout(std430, binding = 1) buffer Out { uint answer[]; };\n"
        "void main() {\n"
        "  for (int i = 0; i < value.length(); ++i) value[i] = uint(i + 1);\n"
        "  uint sum = 0u;\n"
        "  for (int i = 0; i < value.length(); ++i) sum += value[i];\n"
        "  answer[2] = sum;\n"
        "  answer[3] = value[value.length() - 1];\n"
        "}\n");
    if (fill) {
        glUseProgram(fill);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, data);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, out);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    }
    memset(got, 0, sizeof(got));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
    error = glGetError();
    const GLuint want_sum = (GLuint)ELEMENTS * (ELEMENTS + 1) / 2;
    fprintf(stderr, "INFO loop sum=%u want=%u last=%u want=%d err=0x%04x\n",
            got[2], want_sum, got[3], ELEMENTS, (unsigned)error);
    result("loop", fill && error == GL_NO_ERROR && got[2] == want_sum &&
                   got[3] == (GLuint)ELEMENTS);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
