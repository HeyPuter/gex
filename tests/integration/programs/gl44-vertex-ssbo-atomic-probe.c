/* SSBO atomics performed by the *vertex* stage.
 *
 * gl44-ssbo-atomic-probe does its atomics in the fragment stage, where the
 * shader that rasterizes can perform them itself. The vertex stage cannot:
 * WebGPU forbids a written storage binding there, so this driver dispatches
 * the vertex shader as *compute* for its side effects and rasterizes a copy
 * with the writes stripped. Plain stores through that path are covered
 * (gl44-vertex-ssbo-write-probe); atomics are not, and
 * KHR-GL44.shader_storage_buffer_object.basic-atomic-case1 fails on exactly
 * that -- "uData at index 0 is 0 should be 7", a vertex shader's atomics
 * never landing.
 *
 * Each operation is its own case, and the vertex count is chosen so the
 * expected value could not arise from the wrong number of invocations: four
 * vertices, so an add gives 4 and a run of one gives 1. The read-back is a
 * plain glGetBufferSubData after glMemoryBarrier, which is what the CTS does
 * and what an application would do.
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
    fprintf(stderr, "INFO %-38s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define VERTS 4

static const char *fs_src =
    "#version 430 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(0.0, 1.0, 0.0, 1.0); }\n";

/* The vertex shader's body is substituted; `v` is the storage array and
   gl_VertexID distinguishes the invocations. Positions are off-screen on
   purpose for one case below, to prove the side effects do not depend on
   anything being rasterized. */
static GLuint build(const char *body, int offscreen) {
    char vs_src[1200];
    snprintf(vs_src, sizeof vs_src,
             "#version 430 core\n"
             "layout(std430, binding = 0) buffer Out { uint v[]; };\n"
             "void main() {\n"
             "    %s\n"
             "    gl_Position = vec4(%s, 0.0, 1.0);\n"
             "}\n", body, offscreen ? "8.0, 8.0" : "0.0, 0.0");
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    const char *p = vs_src;
    glShaderSource(vs, 1, &p, NULL); glCompileShader(vs);
    GLint ok = 0; glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1200] = {0};
               glGetShaderInfoLog(vs, sizeof log, NULL, log);
               fprintf(stderr, "INFO vs compile failed: %s\n", log);
               return 0; }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1200] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   return 0; }
    return program;
}

/* Run `body` over four vertices and return element `slot` of the buffer. */
static long long run(const char *label, const char *body, unsigned slot,
                     int offscreen) {
    GLuint program = build(body, offscreen);
    if (!program) { fprintf(stderr, "INFO %s: no program\n", label);
                    return -1; }
    GLuint ssbo = 0; glGenBuffers(1, &ssbo);
    const unsigned zeros[8] = {0};
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof zeros, zeros,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
    glUseProgram(program);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_POINTS, 0, VERTS);
    glMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT | GL_SHADER_STORAGE_BARRIER_BIT);
    unsigned got[8] = {0};
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof got, got);
    while (glGetError() != GL_NO_ERROR) { }
    glDeleteBuffers(1, &ssbo);
    glDeleteProgram(program);
    return (long long)got[slot];
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
    GLint blocks = 0;
    glGetIntegerv(GL_MAX_VERTEX_SHADER_STORAGE_BLOCKS, &blocks);
    fprintf(stderr, "INFO GL_MAX_VERTEX_SHADER_STORAGE_BLOCKS=%d\n", blocks);
    if (blocks < 1) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glViewport(0, 0, 16, 16);

    /* One add per invocation: four vertices, so four. */
    eq("vertex_atomic_add", run("add", "atomicAdd(v[0], 1u);", 0, 0), VERTS);
    /* Each invocation sets its own bit: 0b1111 = 15. */
    eq("vertex_atomic_or",
       run("or", "atomicOr(v[0], 1u << uint(gl_VertexID));", 0, 0), 15);
    /* Exchange returns what it replaced; the sum of four ids is 0+1+2+3 = 6
       plus whatever the last exchange left, so only the accumulator is
       checked here -- it is the part that needs the returned value. */
    eq("vertex_atomic_exchange_returns",
       run("exchange",
           "uint was = atomicExchange(v[0], uint(gl_VertexID) + 1u);\n"
           "    atomicAdd(v[1], was);", 1, 0), 0 + 1 + 2 + 3);
    /* Exactly one invocation wins the swap. */
    eq("vertex_atomic_comp_swap_wins_once",
       run("comp-swap",
           "if (atomicCompSwap(v[0], 0u, 1u) == 0u) atomicAdd(v[1], 1u);",
           1, 0), 1);
    /* And with every primitive off-screen, so nothing is rasterized: the
       side effects are the vertex stage's own and must still happen. */
    eq("vertex_atomic_add_offscreen",
       run("offscreen", "atomicAdd(v[0], 1u);", 0, 1), VERTS);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
