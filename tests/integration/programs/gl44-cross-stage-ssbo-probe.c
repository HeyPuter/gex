/* A fragment-stage storage write, when the vertex stage declares the same
 * block. Isolated from gl44-ssbo-probe so the failure can be varied.
 *
 * gl44-ssbo-probe's `cross_stage_storage` case fails in the browser and passes
 * natively, and the readback comes back as the *whole* pre-draw seed -- nobody
 * wrote. The driver's handling of this is deliberate and documented
 * (gex_wgpu_screen.c:40560): read-only in the vertex stage and writable in the
 * fragment stage is a combination WebGPU refuses across a pass, so the vertex
 * side is given a snapshot copy and the fragment side keeps the real buffer.
 * `gex_wgpu_fragment_writes_ssbo` is called only from the vertex group, so the
 * fragment binding is the real buffer by construction.
 *
 * **This bug is masked by every instrument that could measure it.** Passing
 * *any* `mesaenv=` -- even a variable the driver never reads -- makes it pass,
 * because the JS applies host env with a few _malloc/_free calls before
 * _gex_gl_init (web/src/mesa-gl.js:274) and that alone perturbs it. Every
 * driver diagnostic is getenv-gated, so none of them can be switched on
 * without also switching off the bug. Hence this probe: it varies the *GL*
 * calls instead of the driver's environment.
 *
 * What each case separates:
 *   first_attempt   -- does the write ever land?
 *   second_attempt  -- the identical draw again. Passing only here means the
 *                      write lands but is not visible to the readback that
 *                      follows it, which is an ordering fault, not a binding
 *                      one.
 *   after_finish    -- glFinish rather than glMemoryBarrier. Passing only here
 *                      means the barrier is not closing the scope.
 *   through_map     -- read back by mapping instead of glGetBufferSubData, in
 *                      case it is the readback path that is stale.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;
static void eq(const char *name, unsigned got, unsigned want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-22s at1=0x%04x want=0x%04x\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }

static GLuint build(const char *vs_src, const char *fs_src) {
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint p = glCreateProgram();
    glAttachShader(p, vs); glAttachShader(p, fs);
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024]; GLsizei n = 0;
        glGetProgramInfoLog(p, sizeof(log), &n, log);
        fprintf(stderr, "INFO link failed: %.*s\n", (int)n, log);
        return 0;
    }
    return p;
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
    const EGLint sa[] = { EGL_WIDTH, 64, EGL_HEIGHT, 64, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    drain();

    GLuint prog = build(
        "#version 430 core\n"
        "layout(std430, binding = 3) buffer Shared { uint value[]; };\n"
        "out float carried;\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "  carried = float(value[0] & 1u);\n"
        "}\n",
        "#version 430 core\n"
        "layout(std430, binding = 3) buffer Shared { uint value[]; };\n"
        "in float carried;\n"
        "out vec4 colour;\n"
        "void main() {\n"
        "  value[1] = 0xa5a5u;\n"
        "  colour = vec4(carried, 1.0, 0.0, 1.0);\n"
        "}\n");
    if (!prog) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 1; }

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    GLuint buffer = 0;
    const GLuint seed[4] = {0x9999u, 0x8888u, 0x7777u, 0x6666u};
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(seed), seed, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 3, buffer);
    glUseProgram(prog);
    drain();

    /* 0. The case that runs immediately before the failing one in
       gl44-ssbo-probe: one buffer bound to two slots, written through the
       writable binding and read through the read-only one. It exercises the
       same aliasing machinery, so if the failure below is a cache collision
       this is the likeliest partner -- and the failure only appears in the
       family, never alone, which is the collision signature rather than a
       latch (a latch would take every case after it, not one). */
    {
        GLuint alias_prog = build(
            "#version 430 core\n"
            "void main() {\n"
            "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
            "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
            "  gl_Position = vec4(p, 0.0, 1.0); }\n",
            "#version 430 core\n"
            "layout(std430, binding = 0) buffer Written { uint out_value[]; };\n"
            "layout(std430, binding = 1) readonly buffer Read { uint in_value[]; };\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  out_value[1] = 0xd1ceu + (in_value[0] & 0u);\n"
            "  colour = vec4(1.0); }\n");
        GLuint alias_buffer = 0;
        const GLuint alias_seed[4] = {0x1111u, 0x2222u, 0x3333u, 0x4444u};
        glGenBuffers(1, &alias_buffer);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, alias_buffer);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(alias_seed), alias_seed,
                     GL_DYNAMIC_COPY);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, alias_buffer);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, alias_buffer);
        if (alias_prog) {
            glUseProgram(alias_prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
        }
        GLuint alias_back[4] = {0, 0, 0, 0};
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(alias_back),
                           alias_back);
        drain();
        eq("aliased_first", alias_back[1], 0xd1ceu);
        /* Leave the slots as the failing family leaves them. */
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, 0);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, 0);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 3, buffer);
        glUseProgram(prog);
        drain();
    }

    GLuint back[4];
    /* 1. Exactly what gl44-ssbo-probe does. */
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    back[1] = 0; glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(back), back);
    drain();
    eq("first_attempt", back[1], 0xa5a5u);

    /* 2. Reseed and repeat. A pass here but not above is an ordering fault. */
    glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(seed), seed);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    back[1] = 0; glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(back), back);
    drain();
    eq("second_attempt", back[1], 0xa5a5u);

    /* 3. glFinish instead of the barrier. */
    glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(seed), seed);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glFinish();
    back[1] = 0; glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(back), back);
    drain();
    eq("after_finish", back[1], 0xa5a5u);

    /* 4. Read through a mapping rather than glGetBufferSubData. */
    glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(seed), seed);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    glFinish();
    const GLuint *mapped = (const GLuint *)glMapBufferRange(
        GL_SHADER_STORAGE_BUFFER, 0, sizeof(back), GL_MAP_READ_BIT);
    const unsigned via_map = mapped ? mapped[1] : 0xdeadu;
    if (mapped) glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    drain();
    eq("through_map", via_map, 0xa5a5u);

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
