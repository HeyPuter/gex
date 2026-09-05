/* A vertex shader that reads a storage buffer.
 *
 * WGSL forbids `var<storage, read_write>` in the vertex stage outright, and
 * this driver declared every storage block read_write -- so a vertex shader
 * that merely *read* one was refused, and the refusal reached the application
 * as a failed link. That is not an exotic shape: instance data, GPU skinning
 * matrices and draw-parameter tables are all a vertex shader reading a
 * storage buffer, and it is how a modern engine feeds one.
 *
 * The qualifier GLSL sets is only the one the author wrote. `readonly` is
 * optional, so a block nobody writes still arrives writable and has to be
 * *inferred*: collect the blocks that are stored to or atomically updated,
 * and any block not among them is read-only. Mesa's nir_opt_access does this
 * in principle and cannot here -- it identifies the block behind an access
 * with nir_chase_binding, which wants a deref, and by the time this driver
 * sees a shader an access is a load_ssbo with a plain block number. Failing
 * to identify one, it marks every block written.
 *
 *   `reads`     a vertex shader reads four vec4s from a storage block and
 *               positions a quad from them, so the picture is wrong unless
 *               the right values arrived
 *   `two_blocks` one block read by the vertex shader beside one *written* by
 *               the fragment shader. The inference has to be per block: mark
 *               them both writable and the vertex stage is refused again;
 *               mark them both read-only and the fragment write is dropped.
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

#define SIDE 32

/* The corners come out of the block, so nothing is drawn in the right place
 * unless the read worked. */
static const char *vs_read =
    "#version 430 core\n"
    "layout(std430, binding = 0) buffer Corners { vec4 at[4]; } corners;\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(corners.at[gl_VertexID].xy, 0.0, 1.0);\n"
    "  tint = vec3(0.0, 1.0, 0.0);\n"
    "}\n";
/* The same read, but through an *unsized* block that the shader also
 * measures. length() needs the storage variable itself, so unlike an access
 * through a sized block -- lowered to a load_ssbo carrying a block number,
 * after which the variable is dead -- this one survives all the way to the
 * SPIR-V emitter, carrying whatever access qualifier it was given.
 *
 * It is here because that looked like a gap and is not: a block reaching the
 * emitter writable would be declared `var<storage, read_write>`, which WGSL
 * forbids in the vertex stage, and inferring read-only from "nothing stores to
 * it" seemed to be missing. Measured with the inference disabled, the WGSL for
 * this shader already reads `var<storage, read>` -- Mesa sets the qualifier
 * before the driver ever sees the shader. The inference was written twice and
 * reverted twice; this case is what a third attempt should run first.
 *
 * `advanced-unsizedArrayLength-vs-*` in the CTS is this shape plus an output
 * block the vertex shader writes, and those stay blocked on the write itself:
 * a vertex-stage storage *write* is the real WGSL wall, and no qualifier
 * inference reaches it. */
static const char *vs_length =
    "#version 430 core\n"
    "layout(std430, binding = 0) buffer Corners { vec4 at[]; } corners;\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(corners.at[gl_VertexID].xy, 0.0, 1.0);\n"
    "  tint = vec3(0.0, corners.at.length() == 4 ? 1.0 : 0.0, 0.0);\n"
    "}\n";
static const char *fs_plain =
    "#version 430 core\n"
    "in vec3 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(tint, 1.0); }\n";
/* The fragment shader writes a second block; the vertex shader still only
 * reads the first. */
static const char *fs_write =
    "#version 430 core\n"
    "layout(std430, binding = 1) buffer Tally { uint hits; } tally;\n"
    "in vec3 tint;\n"
    "out vec4 colour;\n"
    "void main() {\n"
    "  atomicAdd(tally.hits, 1u);\n"
    "  colour = vec4(tint, 1.0);\n"
    "}\n";

static GLuint build_vs(const char *vs_src, const char *fs_src) {
    struct { GLenum stage; const char *src; } parts[] = {
        {GL_VERTEX_SHADER, vs_src}, {GL_FRAGMENT_SHADER, fs_src},
    };
    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < 2; ++i) {
        GLuint sh = glCreateShader(parts[i].stage);
        glShaderSource(sh, 1, &parts[i].src, NULL);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO compile: %s\n", log);
            glDeleteShader(sh);
            glDeleteProgram(prog);
            return 0;
        }
        glAttachShader(prog, sh);
        glDeleteShader(sh);
    }
    glLinkProgram(prog);
    GLint ok = 0;
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

static GLuint build(const char *fs_src) { return build_vs(vs_read, fs_src); }

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

    /* A triangle strip covering the left half only, so a shader that read
     * zeroes (or nothing) leaves the right half unlike the left. */
    const float corners[16] = {
        -1.0f, -1.0f, 0.0f, 1.0f,
         0.0f, -1.0f, 0.0f, 1.0f,
        -1.0f,  1.0f, 0.0f, 1.0f,
         0.0f,  1.0f, 0.0f, 1.0f,
    };
    GLuint block = 0;
    glGenBuffers(1, &block);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, block);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(corners), corners,
                 GL_STATIC_DRAW);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, block);

    GLuint tally = 0;
    const unsigned zero = 0;
    glGenBuffers(1, &tally);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, tally);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(zero), &zero, GL_STATIC_DRAW);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, tally);

    GLuint prog = build(fs_plain);
    unsigned char left[4] = {0}, right[4] = {0};
    if (prog) {
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(prog);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glReadPixels(SIDE / 4, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, left);
        glReadPixels(3 * SIDE / 4, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     right);
    }
    GLenum error = glGetError();
    fprintf(stderr, "INFO reads left=%u,%u,%u right=%u,%u,%u err=0x%04x\n",
            left[0], left[1], left[2], right[0], right[1], right[2],
            (unsigned)error);
    result("reads", prog && error == GL_NO_ERROR && left[1] > 128 &&
                    right[1] < 64);

    GLuint prog2 = build(fs_write);
    unsigned hits = 0;
    if (prog2) {
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(prog2);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, tally);
        const unsigned *mapped = (const unsigned *)glMapBufferRange(
            GL_SHADER_STORAGE_BUFFER, 0, sizeof(unsigned), GL_MAP_READ_BIT);
        if (mapped) hits = *mapped;
        glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    }
    error = glGetError();
    /* Half of a 32x32 target is 512 fragments; any nonzero count proves the
     * fragment write survived while the vertex read stayed read-only. */
    fprintf(stderr, "INFO two_blocks hits=%u err=0x%04x\n", hits,
            (unsigned)error);
    result("two_blocks", prog2 && error == GL_NO_ERROR && hits > 0);

    GLuint prog3 = build_vs(vs_length, fs_plain);
    unsigned char lit[4] = {0}, unlit[4] = {0};
    if (prog3) {
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(prog3);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glReadPixels(SIDE / 4, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, lit);
        glReadPixels(3 * SIDE / 4, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     unlit);
    }
    error = glGetError();
    /* Green only if length() returned 4, and only on the left half, so this
     * fails both when the block was not read and when it was not measured. */
    fprintf(stderr, "INFO length lit=%u,%u,%u unlit=%u,%u,%u err=0x%04x\n",
            lit[0], lit[1], lit[2], unlit[0], unlit[1], unlit[2],
            (unsigned)error);
    result("length", prog3 && error == GL_NO_ERROR && lit[1] > 128 &&
                     unlit[1] < 64);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
