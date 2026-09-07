/* A dynamically indexed uniform block array, in a *compute* shader.
 *
 * `uniform Block { ... } blocks[N];` indexed by a dynamically uniform
 * expression is ARB_gpu_shader5 and legal in every stage. Everything
 * downstream of the driver's UBO lowering is keyed on a *constant* block
 * number, so a dynamic index is unrolled into a chain of constant loads
 * selected by comparing the index -- `gex_wgpu_lower_dynamic_ubo_index`.
 *
 * That unroll was called from the graphics shader path and nowhere else.
 * Five call sites of the UBO lowering need it, so a compute, geometry or
 * tessellation shader reached the constant-block assumption directly and
 * **failed to link** -- which is the whole program gone, not one draw.
 *
 * The fix is one call per stage, and it moved no CTS score: the case that
 * reaches it in compute then asks for 41 storage buffers where this adapter
 * grants 10. So this probe is the only thing standing between that fix and a
 * silent regression.
 *
 *   `constant_index`   blocks[1] by a literal -- the path that always worked,
 *                      here to prove the unroll did not break it
 *   `dynamic_index`    blocks[i] where i comes from a buffer, so no compiler
 *                      can fold it. Each block holds a distinct value, so a
 *                      driver that picks the wrong one reads the wrong number
 *                      rather than something plausible
 *   `dynamic_all`      every index in turn, which is what catches an unroll
 *                      that serves index 0 and falls through for the rest
 *
 * Four blocks, values 11, 22, 33, 44 -- distinct, and none of them zero, so
 * "read nothing" and "read block 0" are different answers.
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

#define BLOCKS 4

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

    /* One uniform buffer per block, each holding a distinct float. */
    GLuint ubos[BLOCKS] = {0};
    glGenBuffers(BLOCKS, ubos);
    for (int i = 0; i < BLOCKS; ++i) {
        const float value[4] = {11.0f * (i + 1), 0, 0, 0};
        glBindBuffer(GL_UNIFORM_BUFFER, ubos[i]);
        glBufferData(GL_UNIFORM_BUFFER, sizeof(value), value, GL_STATIC_DRAW);
        glBindBufferBase(GL_UNIFORM_BUFFER, i, ubos[i]);
    }

    /* The index comes from a storage buffer, so it cannot be constant-folded. */
    GLuint index_buffer = 0;
    glGenBuffers(1, &index_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, index_buffer);
    const int zero = 0;
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(int), &zero, GL_DYNAMIC_DRAW);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, index_buffer);

    GLuint out_buffer = 0;
    glGenBuffers(1, &out_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
    const float minus[2] = {-1.0f, -1.0f};
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(minus), minus,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, out_buffer);

    static const char *source =
        "#version 430 core\n"
        "layout(local_size_x = 1) in;\n"
        "layout(binding = 0, std140) uniform Block { vec4 v; } blocks[4];\n"
        "layout(std430, binding = 0) buffer Out { float got; float fixed_; } o;\n"
        "layout(std430, binding = 1) buffer In { int index; } i_;\n"
        "void main() {\n"
        "  o.got = blocks[i_.index].v.x;\n"
        "  o.fixed_ = blocks[1].v.x;\n"
        "}\n";

    GLuint program = build_compute(source);
    if (!program) {
        /* The link failing *is* the bug this probe exists for. */
        result("constant_index", 0);
        result("dynamic_index", 0);
        result("dynamic_all", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 0;
    }
    glUseProgram(program);

    int all_ok = 1;
    float first_dynamic = -1.0f, constant_seen = -1.0f;
    for (int i = 0; i < BLOCKS; ++i) {
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, index_buffer);
        glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(int), &i);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT |
                        GL_BUFFER_UPDATE_BARRIER_BIT);
        float got[2] = {-1.0f, -1.0f};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
        const float want = 11.0f * (i + 1);
        fprintf(stderr, "INFO index=%d got=%.1f want=%.1f constant=%.1f\n",
                i, got[0], want, got[1]);
        if (i == 0) { first_dynamic = got[0]; }
        constant_seen = got[1];
        if (got[0] < want - 0.5f || got[0] > want + 0.5f) all_ok = 0;
    }

    result("constant_index", constant_seen > 21.5f && constant_seen < 22.5f);
    result("dynamic_index", first_dynamic > 10.5f && first_dynamic < 11.5f);
    result("dynamic_all", all_ok);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
