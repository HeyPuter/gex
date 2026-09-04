/* Shared memory in a compute shader.
 *
 * `shared` is what makes a compute shader worth writing: a workgroup
 * cooperates through it, and every reduction, prefix sum, tile-based filter
 * and sorting network in a modern renderer is built on it. WGSL has
 * `var<workgroup>` and `workgroupBarrier()`, so this should be a direct
 * translation -- and KHR-GL44's compute_shader.shared-simple,
 * shared-struct and shared-indexing all fail, with no refusal and no
 * validation error, which means the values are wrong rather than the shader
 * being rejected.
 *
 *   `broadcast`  one invocation writes, a barrier, every invocation reads.
 *                The simplest possible use, and the one that fails if the
 *                barrier is not a barrier
 *   `per_lane`   every invocation writes its own slot, a barrier, then
 *                invocation 0 sums the lot -- so a driver where invocations
 *                do not see each other's writes reads its own slot and gets
 *                a much smaller number
 *   `struct_mem` the same through a `shared` struct with an array member,
 *                which is laid out differently and is what shared-struct
 *                exercises
 *   `dynamic`    indexed by a value the compiler cannot fold, which is what
 *                shared-indexing is about: a constant index can be lowered
 *                to registers, a dynamic one cannot
 *
 * The expected sums are chosen so that "every invocation saw only its own
 * write" and "the barrier did nothing" give different wrong answers from
 * each other and from zero.
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

#define LANES 64

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

    GLuint out_buffer = 0;
    glGenBuffers(1, &out_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, out_buffer);
    if (glGetError() != GL_NO_ERROR) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 2;
    }

    /* Sum of 0..63 is 2016; an invocation that saw only its own write would
     * report its own id, and one where the barrier did nothing would report
     * some partial sum -- neither is 2016 and neither is 0. */
    struct { const char *name; const char *body; GLuint want; } tests[] = {
        {"broadcast",
         "shared uint token;\n"
         "void main() {\n"
         "  if (gl_LocalInvocationIndex == 0u) token = 0xabcdu;\n"
         "  barrier();\n"
         /* Every lane reports whether it saw the write; the count must be
          * all of them. */
         "  atomicAdd(value[0], token == 0xabcdu ? 1u : 0u);\n"
         "}\n", LANES},
        {"per_lane",
         "shared uint lane[64];\n"
         "void main() {\n"
         "  lane[gl_LocalInvocationIndex] = gl_LocalInvocationIndex;\n"
         "  barrier();\n"
         "  if (gl_LocalInvocationIndex == 0u) {\n"
         "    uint total = 0u;\n"
         "    for (uint i = 0u; i < 64u; ++i) total += lane[i];\n"
         "    value[0] = total;\n"
         "  }\n"
         "}\n", 2016u},
        {"struct_mem",
         "struct Cell { uint index; vec3 pad; };\n"
         "shared Cell cells[64];\n"
         "void main() {\n"
         "  cells[gl_LocalInvocationIndex].index = gl_LocalInvocationIndex;\n"
         "  cells[gl_LocalInvocationIndex].pad = vec3(0.0);\n"
         "  barrier();\n"
         "  if (gl_LocalInvocationIndex == 0u) {\n"
         "    uint total = 0u;\n"
         "    for (uint i = 0u; i < 64u; ++i) total += cells[i].index;\n"
         "    value[0] = total;\n"
         "  }\n"
         "}\n", 2016u},
        {"dynamic",
         "shared uint lane[64];\n"
         "void main() {\n"
         /* The index comes from the buffer, so nothing can fold it. */
         "  uint at = (gl_LocalInvocationIndex + value[1]) % 64u;\n"
         "  lane[at] = at;\n"
         "  barrier();\n"
         "  if (gl_LocalInvocationIndex == 0u) {\n"
         "    uint total = 0u;\n"
         "    for (uint i = 0u; i < 64u; ++i) total += lane[i];\n"
         "    value[0] = total;\n"
         "  }\n"
         "}\n", 2016u},
    };

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        char source[1400];
        snprintf(source, sizeof(source),
                 "#version 430\n"
                 "layout(local_size_x = %d) in;\n"
                 "layout(std430, binding = 0) buffer Out { uint value[]; };\n"
                 "%s", LANES, tests[i].body);
        GLuint program = build_compute(source);
        /* value[0] accumulates, value[1] is a zero the compiler cannot see. */
        const GLuint seed[4] = {0u, 0u, 0xbeefu, 0xbeefu};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(seed), seed,
                     GL_DYNAMIC_COPY);
        if (program) {
            glUseProgram(program);
            glDispatchCompute(1, 1, 1);
            glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
        }
        GLuint got[4] = {0, 0, 0, 0};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO %s got=%u want=%u err=0x%04x\n", tests[i].name,
                got[0], tests[i].want, (unsigned)error);
        result(tests[i].name, program && error == GL_NO_ERROR &&
                              got[0] == tests[i].want);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
