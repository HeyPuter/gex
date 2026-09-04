/* Shader storage buffers, which is the first rung of the climb above GL 3.3.
 *
 * GL_ARB_shader_storage_buffer_object is GL 4.3, and Mesa exposes it on the
 * strength of one number: `Program[MESA_SHADER_FRAGMENT].MaxShaderStorageBlocks`,
 * which comes from this driver's `caps->max_shader_buffers`. It is also on the
 * dependency path to compute -- st_extensions gates ARB_compute_shader behind
 * image load/store and atomic counters, and those behind storage of their own
 * -- so nothing above 3.3 that modern applications actually use starts before
 * this works.
 *
 * WebGPU has storage buffers natively, which is why this is the rung to take
 * first: unlike polygon mode, logic ops or wide lines, it is a feature the
 * target *has* rather than one it lacks. The driver already binds storage
 * buffers for transform feedback, for its own capture passes, and for the
 * uniform blocks that overflow WebGPU's twelve-uniform limit.
 *
 * One asymmetry to respect rather than paper over: WebGPU forbids a *writable*
 * storage binding in the vertex stage. Read-only is allowed there, and both
 * are allowed in the fragment stage. GL permits writes from every stage, so a
 * vertex-stage write is the one case this cannot serve.
 *
 * The cases build up rather than testing one big thing:
 *
 *   extension_present  is it advertised at all
 *   fragment_read      a fragment shader reads a value the CPU put there
 *   fragment_write     a fragment shader writes, and the CPU reads it back
 *   vertex_read        the vertex stage reads one (read-only is legal there)
 *   two_blocks         two bindings at once, so slot and binding cannot be
 *                      confused for one another
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

#define SIDE 32

static int has_extension(const char *want) {
    GLint n = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &n);
    if (glGetError() != GL_NO_ERROR) return 0;
    for (GLint i = 0; i < n; ++i) {
        const char *e = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (e && !strcmp(e, want)) return 1;
    }
    return 0;
}

/* A compute program, for the aliasing case below -- the graphics build()
 * beneath this one takes a vertex and fragment pair. */
static GLuint build_compute(const char *src) {
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, &src, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compute compile: %s\n", log);
        return 0;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compute link: %s\n", log);
        return 0;
    }
    return program;
}

static GLuint build(const char *vs_src, const char *fs_src) {
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(vs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO vs compile: %s\n", log);
        return 0;
    }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO fs compile: %s\n", log);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return prog;
}

/* A full-target triangle pair, so every case covers the whole framebuffer. */
static void draw_quad(void) {
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
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

    GLint max_frag_blocks = 0, max_vert_blocks = 0, max_size = 0;
    glGetIntegerv(GL_MAX_FRAGMENT_SHADER_STORAGE_BLOCKS, &max_frag_blocks);
    glGetIntegerv(GL_MAX_VERTEX_SHADER_STORAGE_BLOCKS, &max_vert_blocks);
    glGetIntegerv(GL_MAX_SHADER_STORAGE_BLOCK_SIZE, &max_size);
    const int present = has_extension("GL_ARB_shader_storage_buffer_object");
    /* Errors from the queries above are expected when the extension is
     * absent; clear them so they cannot be mistaken for a later failure. */
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO extension present=%d frag_blocks=%d vert_blocks=%d "
            "max_block_size=%d version=%s glsl=%s\n", present, max_frag_blocks,
            max_vert_blocks, max_size, glGetString(GL_VERSION),
            glGetString(GL_SHADING_LANGUAGE_VERSION));
    result("extension_present", present && max_frag_blocks > 0);
    if (!present) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    GLuint colour = 0, fbo = 0;
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
    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    static const char *vs_plain =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";

    /* Reading a value the CPU wrote. 64 is chosen so the answer is a colour
     * that cannot be confused with black, white or the clear. */
    {
        static const char *fs =
            "#version 330 core\n"
            "#extension GL_ARB_shader_storage_buffer_object : require\n"
            "layout(std430, binding = 0) buffer Data { uint value[]; };\n"
            "out vec4 colour;\n"
            "void main() { colour = vec4(float(value[1]) / 255.0, 0, 0, 1); }\n";
        GLuint prog = build(vs_plain, fs);
        GLuint ssbo = 0;
        const unsigned data[4] = {11u, 64u, 22u, 33u};
        glGenBuffers(1, &ssbo);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(data), data,
                     GL_STATIC_DRAW);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
        /* Read the buffer straight back before any draw. A storage buffer is
         * read from the device rather than from the CPU shadow, so this says
         * whether glBufferData's contents ever reached the GPU -- which is a
         * different failure from the shader reading the wrong place. */
        unsigned echo[4] = {0};
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(echo), echo);
        fprintf(stderr, "INFO fragment_read upload echo=%u,%u,%u,%u "
                "want=11,64,22,33\n", echo[0], echo[1], echo[2], echo[3]);
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) { glUseProgram(prog); draw_quad(); }
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO fragment_read got=%u want=64 err=0x%04x\n",
                px[0], (unsigned)error);
        result("fragment_read", prog && error == GL_NO_ERROR &&
                                px[0] >= 62 && px[0] <= 66);
        glDeleteBuffers(1, &ssbo);
    }

    /* Writing from the fragment stage, read back with glGetBufferSubData.
     * Every invocation writes the same value to the same slot, so the result
     * does not depend on how many fragments ran -- only on whether any write
     * arrived at all. */
    {
        static const char *fs =
            "#version 330 core\n"
            "#extension GL_ARB_shader_storage_buffer_object : require\n"
            "layout(std430, binding = 0) buffer Data { uint value[]; };\n"
            "out vec4 colour;\n"
            "void main() { value[0] = 77u; colour = vec4(0, 1, 0, 1); }\n";
        GLuint prog = build(vs_plain, fs);
        GLuint ssbo = 0;
        const unsigned data[4] = {0u, 0u, 0u, 0u};
        glGenBuffers(1, &ssbo);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(data), data,
                     GL_DYNAMIC_COPY);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) { glUseProgram(prog); draw_quad(); }
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
        unsigned readback[4] = {0};
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(readback),
                           readback);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO fragment_write got=%u want=77 err=0x%04x\n",
                readback[0], (unsigned)error);
        result("fragment_write",
               prog && error == GL_NO_ERROR && readback[0] == 77u);
        glDeleteBuffers(1, &ssbo);
    }

    /* The vertex stage reading one. Read-only is what WebGPU allows there, so
     * this is the vertex case that has to work. */
    {
        static const char *vs =
            "#version 330 core\n"
            "#extension GL_ARB_shader_storage_buffer_object : require\n"
            "layout(std430, binding = 0) buffer Data { uint value[]; };\n"
            "flat out uint carried;\n"
            "void main() {\n"
            "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
            "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
            "  carried = value[2];\n"
            "  gl_Position = vec4(p, 0.0, 1.0);\n"
            "}\n";
        static const char *fs =
            "#version 330 core\n"
            "flat in uint carried;\n"
            "out vec4 colour;\n"
            "void main() { colour = vec4(float(carried) / 255.0, 0, 0, 1); }\n";
        GLuint prog = build(vs, fs);
        GLuint ssbo = 0;
        const unsigned data[4] = {1u, 2u, 128u, 4u};
        glGenBuffers(1, &ssbo);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(data), data,
                     GL_STATIC_DRAW);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) { glUseProgram(prog); draw_quad(); }
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO vertex_read got=%u want=128 err=0x%04x\n", px[0],
                (unsigned)error);
        result("vertex_read", prog && error == GL_NO_ERROR &&
                              px[0] >= 126 && px[0] <= 130);
        glDeleteBuffers(1, &ssbo);
    }

    /* Two blocks at once. One binding is not enough to tell a driver that
     * confuses the block's slot with its binding from one that does not. */
    {
        static const char *fs =
            "#version 330 core\n"
            "#extension GL_ARB_shader_storage_buffer_object : require\n"
            "layout(std430, binding = 0) buffer A { uint a[]; };\n"
            "layout(std430, binding = 1) buffer B { uint b[]; };\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  colour = vec4(float(a[0]) / 255.0, float(b[0]) / 255.0, 0, 1);\n"
            "}\n";
        GLuint prog = build(vs_plain, fs);
        GLuint ssbo[2] = {0, 0};
        const unsigned a[4] = {40u, 0, 0, 0};
        const unsigned b[4] = {200u, 0, 0, 0};
        glGenBuffers(2, ssbo);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo[0]);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(a), a, GL_STATIC_DRAW);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo[0]);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo[1]);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(b), b, GL_STATIC_DRAW);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, ssbo[1]);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) { glUseProgram(prog); draw_quad(); }
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO two_blocks got=%u,%u want=40,200 err=0x%04x\n",
                px[0], px[1], (unsigned)error);
        result("two_blocks", prog && error == GL_NO_ERROR &&
                             px[0] >= 38 && px[0] <= 42 &&
                             px[1] >= 198 && px[1] <= 202);
        glDeleteBuffers(2, ssbo);
    }

    /* One buffer bound to two storage slots at once, written through one and
     * read through the other. GL allows it -- the values read are undefined
     * without a barrier, but the binding and the write are not -- and WebGPU
     * refuses any writable binding that shares a synchronization scope with
     * another use of the same buffer:
     *
     *   [Buffer] usage (Storage(read-write)|Storage(read-only)) includes
     *   writable usage and another usage in the same synchronization scope
     *
     * naming neither binding. Blender's EEVEE binds one buffer to three slots
     * of its light-culling dispatch. The read-only side takes a copy now, so
     * what this asserts is the part GL does define: the *write* lands, and
     * the draw is not dropped. Nothing is asserted about the value read
     * through the other binding, because GL leaves it undefined and a test
     * that asserted on it would be a test that lies. */
    {
        GLuint prog = build_compute(
            "#version 430\n"
            "layout(local_size_x = 1) in;\n"
            "layout(std430, binding = 0) buffer Written { uint out_value[]; };\n"
            "layout(std430, binding = 1) readonly buffer Read { uint in_value[]; };\n"
            "void main() {\n"
            "  out_value[1] = 0xd1ceu + (in_value[0] & 0u);\n"
            "}\n");
        GLuint buffer = 0;
        GLuint seed[4] = {0x1111u, 0x2222u, 0x3333u, 0x4444u};
        glGenBuffers(1, &buffer);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(seed), seed,
                     GL_DYNAMIC_COPY);
        /* The same buffer at both slots. */
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buffer);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, buffer);
        if (prog) {
            glUseProgram(prog);
            glDispatchCompute(1, 1, 1);
            glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
        }
        GLuint back[4] = {0, 0, 0, 0};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(back), back);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO aliased_storage at1=0x%x at0=0x%x want=0xd1ce,"
                "0x1111 err=0x%04x\n", back[1], back[0], (unsigned)error);
        result("aliased_storage", prog && error == GL_NO_ERROR &&
                                  back[1] == 0xd1ceu && back[0] == 0x1111u);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, 0);
        glDeleteBuffers(1, &buffer);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
