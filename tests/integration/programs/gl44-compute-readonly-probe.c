/* A compute shader that only reads a storage block, and one that declares a
 * block nothing is bound to.
 *
 * Both were fatal and neither had coverage. gl44-compute-probe stops earlier,
 * at the deliberate fragment-only shader-image cap, so everything past that --
 * including every SSBO case -- had never run on this driver.
 *
 * 1. A block the shader never writes is declared `var<storage, read>` by the
 *    SPIR-V emitter, and WebGPU requires the bind group layout to agree:
 *
 *      The buffer type in the shader (BufferBindingType::ReadOnlyStorage) is
 *      not compatible with the type in the layout (BufferBindingType::Storage)
 *
 *    launch_grid said Storage for every storage block, so any compute shader
 *    with a read-only block produced an invalid pipeline -- and an invalid
 *    pipeline makes the whole command buffer invalid, so the submit fails and
 *    everything recorded with it is lost. The graphics path had carried this
 *    bit per stage for a long time; compute had no equivalent.
 *
 * 2. A block with nothing bound has no WebGPU expression at all -- a declared
 *    binding must have a buffer behind it -- and the driver failed the context,
 *    which turns one unbound block into GL_OUT_OF_MEMORY from every later
 *    call. GL leaves the *contents* undefined, and this driver advertises
 *    robust_buffer_access_behavior under which the read is zero, so a
 *    placeholder is the answer. The case below therefore checks that the
 *    dispatch does not fail and that the *bound* block still gets the right
 *    answer -- not what the unbound one reads, which no two drivers need to
 *    agree on.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define COUNT 8

static GLuint build(const char *source)
{
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return program;
}

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
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint in_buffer = 0, out_buffer = 0;
    GLuint input[COUNT], zeros[COUNT];
    for (unsigned i = 0; i < COUNT; ++i) {
        input[i] = 100u + i * 7u;
        zeros[i] = 0;
    }
    glGenBuffers(1, &in_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, in_buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(input), input,
                 GL_STATIC_DRAW);
    glGenBuffers(1, &out_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(zeros), zeros,
                 GL_DYNAMIC_COPY);

    /* The block is never written, so nir_opt_access marks it non-writeable
     * whether or not the `readonly` qualifier is present -- both spellings are
     * covered because only one of them is visible in the GLSL. */
    static const char *const sources[2] = {
        "#version 430\n"
        "layout(local_size_x = 1) in;\n"
        "layout(std430, binding = 0) readonly buffer In { uint v[]; };\n"
        "layout(std430, binding = 1) buffer Out { uint r[]; };\n"
        "void main() { r[gl_GlobalInvocationID.x] ="
        " v[gl_GlobalInvocationID.x] * 3u + 1u; }\n",

        "#version 430\n"
        "layout(local_size_x = 1) in;\n"
        "layout(std430, binding = 0) buffer In { uint v[]; };\n"
        "layout(std430, binding = 1) buffer Out { uint r[]; };\n"
        "void main() { r[gl_GlobalInvocationID.x] ="
        " v[gl_GlobalInvocationID.x] * 3u + 1u; }\n",
    };
    static const char *const names[2] = {
        "readonly-qualified", "readonly-inferred",
    };

    for (unsigned t = 0; t < 2; ++t) {
        GLuint program = build(sources[t]);
        GLuint got[COUNT];
        memset(got, 0xff, sizeof(got));
        GLenum error = GL_NO_ERROR;
        int ok = 0;
        if (program) {
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
            glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(zeros), zeros);
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, in_buffer);
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, out_buffer);
            glUseProgram(program);
            glDispatchCompute(COUNT, 1, 1);
            glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT |
                            GL_BUFFER_UPDATE_BARRIER_BIT);
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
            glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
            error = glGetError();
            ok = error == GL_NO_ERROR;
            for (unsigned i = 0; i < COUNT && ok; ++i)
                ok = got[i] == input[i] * 3u + 1u;
        }
        fprintf(stderr, "INFO %s got=%u,%u want=%u,%u err=0x%04x\n", names[t],
                got[0], got[1], input[0] * 3u + 1u, input[1] * 3u + 1u,
                (unsigned)error);
        result(names[t], ok);
        if (program) glDeleteProgram(program);
    }

    /* A third block the shader reads and the application never binds. The
     * bound one must still come out right, and nothing may fail. */
    {
        GLuint program = build(
            "#version 430\n"
            "layout(local_size_x = 1) in;\n"
            "layout(std430, binding = 0) readonly buffer In { uint v[]; };\n"
            "layout(std430, binding = 1) buffer Out { uint r[]; };\n"
            "layout(std430, binding = 2) readonly buffer Absent { uint a[]; };\n"
            "void main() {\n"
            "  uint extra = a[gl_GlobalInvocationID.x] & 0u;\n"
            "  r[gl_GlobalInvocationID.x] ="
            " v[gl_GlobalInvocationID.x] * 3u + 1u + extra;\n"
            "}\n");
        GLuint got[COUNT];
        memset(got, 0xff, sizeof(got));
        GLenum error = GL_NO_ERROR;
        int ok = 0;
        if (program) {
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
            glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(zeros), zeros);
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, in_buffer);
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, out_buffer);
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 2, 0);
            glUseProgram(program);
            glDispatchCompute(COUNT, 1, 1);
            glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT |
                            GL_BUFFER_UPDATE_BARRIER_BIT);
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
            glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
            error = glGetError();
            /* `& 0u` makes the unbound block's contents irrelevant, so this
             * is about surviving the dispatch, not about what it read. */
            ok = error == GL_NO_ERROR;
            for (unsigned i = 0; i < COUNT && ok; ++i)
                ok = got[i] == input[i] * 3u + 1u;
        }
        fprintf(stderr, "INFO unbound-block got=%u want=%u err=0x%04x\n",
                got[0], input[0] * 3u + 1u, (unsigned)error);
        result("unbound-block", ok);
        if (program) glDeleteProgram(program);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
