/* Shader storage atomics, and GL's atomic counters on top of them.
 *
 * Not a ladder item -- `ARB_shader_atomic_counters` has been *advertised*
 * since shader storage buffers landed, because st/mesa derives it from them --
 * which makes this a hole in a claim already made rather than a missing
 * feature. A shader using either fails to link:
 *
 *     gex_wgpu requires vec4-backed, 16-byte-aligned Gallium UBO loads:
 *     shader storage atomics is not implemented
 *
 * It is not obscure. Order-independent transparency allocates its per-pixel
 * list nodes with an atomic counter; GPU-driven culling appends visible
 * instances with an atomic add; every occupancy or histogram pass is atomics
 * and nothing else. And it is the reason `gl44-fbo-no-attachments-probe`
 * reported zero fragments for a framebuffer that was working perfectly: the
 * obvious way to count fragments is an atomic add, and the failure named
 * neither feature.
 *
 * WGSL has atomics, on a different declaration: `atomic<u32>` in the storage
 * address space, which may *only* be reached through atomic builtins. A plain
 * word array may not be atomically updated and an atomic may not be plainly
 * loaded, so the last case below -- a plain store and an atomic add to the
 * same buffer, which GL allows freely -- is the one that says whether the
 * mapping is real or only covers the easy shape.
 *
 *   `add`        one atomicAdd per fragment; the total is the fragment count
 *   `min-max`    atomicMin and atomicMax over values that arrive in no
 *                particular order, so only the extremes can survive
 *   `bitwise`    atomicAnd, atomicOr and atomicXor
 *   `exchange`   atomicExchange returns the value it replaced
 *   `comp-swap`  atomicCompSwap, which succeeds exactly once across every
 *                fragment -- a race with one winner, which is what makes it
 *                the useful primitive
 *   `counter`    GL's own atomic counter, atomicCounterIncrement
 *   `mixed`      a plain store and an atomic add to the same block
 *
 * Every count is exact. A driver that serialises the shader, drops the
 * atomicity or lowers an atomic to a read-modify-write reads back a smaller
 * number than the fragment count, which is the failure that matters.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 16
#define FRAGMENTS (SIDE * SIDE)

static int have_extension(const char *want)
{
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

static const char *quad_vs =
    "#version 330 core\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
    "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "}\n";

static GLuint link(const char *fs_body, const char *decls)
{
    char fs_src[1400];
    snprintf(fs_src, sizeof(fs_src),
             "#version 330 core\n"
             "#extension GL_ARB_shader_storage_buffer_object : require\n"
             "%s\n"
             "out vec4 colour;\n"
             "void main() {\n"
             "  %s\n"
             "  colour = vec4(0.0, 0.0, 0.0, 1.0);\n"
             "}\n", decls, fs_body);
    const char *fs_ptr = fs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &quad_vs, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_ptr, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        glDeleteShader(vs);
        glDeleteShader(fs);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glDeleteShader(vs);
    glDeleteShader(fs);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

static GLuint ssbo;
#define WORDS 8

/* Seed the buffer, run one full-viewport quad, read the words back. */
static int run(GLuint prog, const uint32_t *seed, uint32_t *out)
{
    if (!prog) return 0;
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, WORDS * 4, seed);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
    glUseProgram(prog);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    const uint32_t *read = (const uint32_t *)glMapBufferRange(
        GL_SHADER_STORAGE_BUFFER, 0, WORDS * 4, GL_MAP_READ_BIT);
    if (!read) return 0;
    memcpy(out, read, WORDS * 4);
    glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    return 1;
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
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have_ssbo =
        have_extension("GL_ARB_shader_storage_buffer_object");
    const int have_counters =
        have_extension("GL_ARB_shader_atomic_counters");
    fprintf(stderr, "INFO version=%s ssbo=%d atomic_counters=%d\n",
            glGetString(GL_VERSION), have_ssbo, have_counters);

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);
    glGenBuffers(1, &ssbo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    {
        uint32_t zero[WORDS] = {0};
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(zero), zero,
                     GL_DYNAMIC_COPY);
    }
    while (glGetError() != GL_NO_ERROR) {}

    static const char *block =
        "layout(std430, binding = 0) buffer B { uint v[8]; };";

    /* Case 1: one add per fragment. */
    {
        GLuint prog = have_ssbo ? link("atomicAdd(v[0], 1u);", block) : 0;
        uint32_t seed[WORDS] = {0}, got[WORDS] = {0};
        const int ran = run(prog, seed, got);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO add got=%u want=%u err=0x%04x\n", got[0],
                FRAGMENTS, (unsigned)error);
        result("add", ran && error == GL_NO_ERROR && got[0] == FRAGMENTS);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 2: min and max over the fragment coordinates, whose order is not
     * defined -- so only the true extremes can be left. */
    {
        GLuint prog = have_ssbo ? link(
            "uint id = uint(gl_FragCoord.y) * 16u + uint(gl_FragCoord.x);\n"
            "  atomicMin(v[0], id);\n"
            "  atomicMax(v[1], id);", block) : 0;
        uint32_t seed[WORDS] = {0}, got[WORDS] = {0};
        seed[0] = 0xffffffffu;
        seed[1] = 0;
        const int ran = run(prog, seed, got);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO min-max got=%u,%u want=0,%u err=0x%04x\n",
                got[0], got[1], FRAGMENTS - 1, (unsigned)error);
        result("min-max", ran && error == GL_NO_ERROR && got[0] == 0 &&
                          got[1] == (uint32_t)(FRAGMENTS - 1));
        if (prog) glDeleteProgram(prog);
    }

    /* Case 3: the bitwise three. Every fragment contributes the same bits, so
     * the result does not depend on the order -- what it depends on is the
     * operation being the right one. */
    {
        GLuint prog = have_ssbo ? link(
            "atomicAnd(v[0], 0x0f0f0f0fu);\n"
            "  atomicOr(v[1], 0x00ff00ffu);\n"
            "  atomicXor(v[2], 0u);", block) : 0;
        uint32_t seed[WORDS] = {0}, got[WORDS] = {0};
        seed[0] = 0xffffffffu;
        seed[1] = 0xf000000fu;
        seed[2] = 0x12345678u;
        const int ran = run(prog, seed, got);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO bitwise got=%08x,%08x,%08x "
                "want=0f0f0f0f,f0ff00ff,12345678 err=0x%04x\n", got[0], got[1],
                got[2], (unsigned)error);
        result("bitwise", ran && error == GL_NO_ERROR &&
                          got[0] == 0x0f0f0f0fu && got[1] == 0xf0ff00ffu &&
                          got[2] == 0x12345678u);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 4: exchange. Each fragment swaps in its own id and adds up what it
     * got back; the sum of everything replaced is the seed plus every id but
     * the last one written, whatever the order -- so the invariant checked is
     * that the total of (returned values) + (final value) equals the seed plus
     * the sum of all ids. That holds for any interleaving and fails for a
     * dropped exchange. */
    {
        GLuint prog = have_ssbo ? link(
            "uint id = uint(gl_FragCoord.y) * 16u + uint(gl_FragCoord.x) + 1u;"
            "\n  uint was = atomicExchange(v[0], id);\n"
            "  atomicAdd(v[1], was);", block) : 0;
        uint32_t seed[WORDS] = {0}, got[WORDS] = {0};
        seed[0] = 7;
        const int ran = run(prog, seed, got);
        const GLenum error = glGetError();
        uint32_t all = 7;
        for (unsigned i = 1; i <= FRAGMENTS; ++i) all += i;
        fprintf(stderr, "INFO exchange final=%u returned=%u sum=%u want=%u "
                "err=0x%04x\n", got[0], got[1], got[0] + got[1], all,
                (unsigned)error);
        result("exchange", ran && error == GL_NO_ERROR &&
                           got[0] + got[1] == all);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 5: compare-and-swap, which must succeed exactly once. Every
     * fragment tries to claim the slot; the one that does increments a
     * counter, so the counter has to read 1 -- a race with a single winner,
     * which is the whole reason the primitive exists. */
    {
        GLuint prog = have_ssbo ? link(
            "if (atomicCompSwap(v[0], 0u, 1u) == 0u) atomicAdd(v[1], 1u);",
            block) : 0;
        uint32_t seed[WORDS] = {0}, got[WORDS] = {0};
        const int ran = run(prog, seed, got);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO comp-swap claimed=%u winners=%u want=1,1 "
                "err=0x%04x\n", got[0], got[1], (unsigned)error);
        result("comp-swap", ran && error == GL_NO_ERROR && got[0] == 1 &&
                            got[1] == 1);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 6: GL's own atomic counter, which Mesa lowers onto storage
     * atomics -- so it exercises the same path through a different front
     * door, and it is the thing ARB_shader_atomic_counters actually
     * advertises. */
    {
        static const char *counter_fs =
            "#version 330 core\n"
            "#extension GL_ARB_shader_atomic_counters : require\n"
            "layout(binding = 0, offset = 0) uniform atomic_uint counter;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  atomicCounterIncrement(counter);\n"
            "  colour = vec4(0.0, 0.0, 0.0, 1.0);\n"
            "}\n";
        GLuint prog = 0, buffer = 0;
        uint32_t got = 0xffffffffu;
        if (have_counters) {
            GLuint vs = glCreateShader(GL_VERTEX_SHADER);
            glShaderSource(vs, 1, &quad_vs, NULL);
            glCompileShader(vs);
            GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
            glShaderSource(fs, 1, &counter_fs, NULL);
            glCompileShader(fs);
            GLint ok = 0;
            glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
            if (!ok) {
                char log[900] = {0};
                glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
                fprintf(stderr, "INFO counter compile: %s\n", log);
            } else {
                prog = glCreateProgram();
                glAttachShader(prog, vs);
                glAttachShader(prog, fs);
                glLinkProgram(prog);
                glGetProgramiv(prog, GL_LINK_STATUS, &ok);
                if (!ok) {
                    char log[900] = {0};
                    glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
                    fprintf(stderr, "INFO counter link: %s\n", log);
                    glDeleteProgram(prog);
                    prog = 0;
                }
            }
            glDeleteShader(vs);
            glDeleteShader(fs);
        }
        if (prog) {
            const uint32_t zero = 0;
            glGenBuffers(1, &buffer);
            glBindBuffer(GL_ATOMIC_COUNTER_BUFFER, buffer);
            glBufferData(GL_ATOMIC_COUNTER_BUFFER, sizeof(zero), &zero,
                         GL_DYNAMIC_COPY);
            glBindBufferBase(GL_ATOMIC_COUNTER_BUFFER, 0, buffer);
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_ATOMIC_COUNTER_BARRIER_BIT);
            const uint32_t *read = (const uint32_t *)glMapBufferRange(
                GL_ATOMIC_COUNTER_BUFFER, 0, sizeof(zero), GL_MAP_READ_BIT);
            if (read) {
                got = read[0];
                glUnmapBuffer(GL_ATOMIC_COUNTER_BUFFER);
            }
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO counter got=%u want=%u err=0x%04x\n", got,
                FRAGMENTS, (unsigned)error);
        result("counter", have_counters && prog && error == GL_NO_ERROR &&
                          got == FRAGMENTS);
        if (prog) glDeleteProgram(prog);
        if (buffer) glDeleteBuffers(1, &buffer);
    }

    /* Case 7: a plain store and an atomic add to the same block. GL allows
     * this freely; WGSL's atomic<u32> may only be reached through atomic
     * builtins and a plain word array may not be atomically updated, so this
     * is the case that says whether the mapping is real. */
    {
        GLuint prog = have_ssbo ? link(
            "atomicAdd(v[0], 1u);\n"
            "  v[3] = 0xabcdu;\n"
            "  uint seen = v[4];\n"
            "  atomicAdd(v[5], seen);", block) : 0;
        uint32_t seed[WORDS] = {0}, got[WORDS] = {0};
        seed[4] = 3;
        const int ran = run(prog, seed, got);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO mixed atomic=%u stored=0x%x summed=%u "
                "want=%u,0xabcd,%u err=0x%04x\n", got[0], got[3], got[5],
                FRAGMENTS, FRAGMENTS * 3, (unsigned)error);
        result("mixed", ran && error == GL_NO_ERROR && got[0] == FRAGMENTS &&
                        got[3] == 0xabcdu && got[5] == FRAGMENTS * 3);
        if (prog) glDeleteProgram(prog);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
