/* A uniform block array, with an explicit binding, read from a compute shader.
 *
 * KHR-GL44.shading_language_420pack.binding_uniform_block_array runs its block
 * array through every shader stage. The vertex stage is right
 * (gl44-block-array-binding-probe, 5/5 both paths) and so is the emulated
 * geometry stage (gl44-gs-block-array-probe, 6/6 both paths, data included),
 * which leaves tessellation and compute. This is compute, the cheaper of the
 * two: no patch, no control/evaluation pair, and an SSBO makes the answer a
 * number rather than a pixel.
 *
 * Same weighting trick as the geometry probe: the three blocks hold 1, 2 and 3
 * and are summed as 1x, 2x and 4x, so 17 means all three were read in the
 * right order, 0 means they were not reached, and any other value names which
 * one went wrong.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-38s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *cs_src =
    "#version 430 core\n"
    "layout(local_size_x = 1, local_size_y = 1, local_size_z = 1) in;\n"
    "layout(std140, binding = 2) uniform GOKU {\n"
    "    vec4 value;\n"
    "} goku[3];\n"
    "layout(std430, binding = 0) buffer OUT { float sum; } result;\n"
    "void main() {\n"
    "    result.sum = goku[0].value.x + goku[1].value.x * 2.0\n"
    "               + goku[2].value.x * 4.0;\n"
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
    glShaderSource(cs, 1, &cs_src, NULL); glCompileShader(cs);
    GLint ok = 0; glGetShaderiv(cs, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024] = {0};
               glGetShaderInfoLog(cs, sizeof log, NULL, log);
               fprintf(stderr, "INFO cs compile failed: %s\n", log);
               printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    GLuint program = glCreateProgram();
    glAttachShader(program, cs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    ++cases; ++passed;
    printf("CASE cs_block_array_links PASS\n");

    static const char *names[3] = { "GOKU[0]", "GOKU[1]", "GOKU[2]" };
    static const char *labels[3] = { "cs_goku0_binding_is_2",
                                     "cs_goku1_binding_is_3",
                                     "cs_goku2_binding_is_4" };
    for (unsigned i = 0; i < 3; ++i) {
        const GLuint index = glGetUniformBlockIndex(program, names[i]);
        GLint binding = -1;
        if (index != GL_INVALID_INDEX)
            glGetActiveUniformBlockiv(program, index, GL_UNIFORM_BLOCK_BINDING,
                                      &binding);
        eq(labels[i], binding, 2 + i);
    }

    GLuint ubo[3] = {0, 0, 0};
    glGenBuffers(3, ubo);
    const float values[3] = { 1.0f, 2.0f, 3.0f };
    for (unsigned i = 0; i < 3; ++i) {
        float block[4] = { values[i], 0.0f, 0.0f, 0.0f };
        glBindBuffer(GL_UNIFORM_BUFFER, ubo[i]);
        glBufferData(GL_UNIFORM_BUFFER, sizeof block, block, GL_STATIC_DRAW);
        glBindBufferBase(GL_UNIFORM_BUFFER, 2 + i, ubo[i]);
    }
    GLuint out = 0; glGenBuffers(1, &out);
    const float zero = 0.0f;
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof zero, &zero, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, out);
    glUseProgram(program);
    eq("cs_setup_is_clean", drain(), GL_NO_ERROR);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    eq("cs_dispatch_is_clean", drain(), GL_NO_ERROR);

    float got = -1.0f;
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof got, &got);
    eq("cs_read_all_three_blocks", (long long)got, 17);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
