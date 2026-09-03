/* ARB_robust_buffer_access_behavior -- one of GL 4.3's five remaining items.
 *
 * The extension is a *promise*, not a feature: that an out-of-bounds buffer
 * access from a shader cannot crash, cannot read another buffer's contents,
 * and cannot write outside the buffer it addresses. Either the access is
 * dropped, or it returns zero, or it returns something from within the same
 * buffer -- the specification allows all three and requires nothing more.
 *
 * WebGPU makes exactly this guarantee by construction. Its specification
 * defines every out-of-bounds buffer access as returning zero or being
 * discarded, and an implementation that let one escape would be a security
 * bug rather than a conformance one. So this is a claim the target already
 * satisfies -- which makes it the cheapest of the five and worth checking
 * rather than assuming, because "the platform promises it" and "the driver
 * does not defeat the promise" are different claims. A driver that clamps an
 * index on the CPU with signed arithmetic, or that sizes a binding from the
 * shader's declaration rather than the buffer, breaks it.
 *
 * Since the returned value is undefined, a probe cannot check the value. What
 * it *can* check -- and what the extension actually guarantees -- is that
 * nothing leaks and nothing is corrupted, so every case below pairs the
 * out-of-bounds access with a **sentinel** in a neighbouring buffer:
 *
 *   `ssbo-read`      read far past the end of a storage buffer; the sentinel
 *                    must not come back
 *   `ssbo-write`     write far past the end of one; the sentinel must survive
 *   `ubo-read`       the same read through a uniform block, which is a
 *                    different binding type with a different bounds path
 *   `buftex-fetch`   texelFetch past the end of a texture buffer, which this
 *                    driver serves as a storage read and so bounds itself
 *   `vertex-fetch`   a draw whose indices run past the end of the vertex
 *                    buffer, which is the fixed-function half of the same
 *                    promise
 *
 * A crash fails all five, which is the point: this probe is as much a memory
 * safety test as a conformance one.
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

#define SIDE 8
/* The value planted in the neighbouring buffer. Distinctive on purpose: if it
 * comes back from an out-of-bounds read, the read escaped its buffer. */
#define SENTINEL 0x5ca1ab1e

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

static GLuint compile(GLenum stage, const char *src)
{
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL);
    glCompileShader(sh);
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        glDeleteShader(sh);
        return 0;
    }
    return sh;
}

static GLuint link_pair(const char *vs_src, const char *fs_src)
{
    GLuint vs = compile(GL_VERTEX_SHADER, vs_src);
    GLuint fs = vs ? compile(GL_FRAGMENT_SHADER, fs_src) : 0;
    if (!vs || !fs) {
        if (vs) glDeleteShader(vs);
        if (fs) glDeleteShader(fs);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glDeleteShader(vs);
    glDeleteShader(fs);
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

static const char *quad_vs =
    "#version 330 core\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
    "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "}\n";

/* Draw a full-screen quad and return the centre pixel's four bytes. */
static void draw_and_read(GLuint prog, unsigned char out[4])
{
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    if (prog) {
        glUseProgram(prog);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    }
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, out);
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

    const int have = have_extension("GL_ARB_robust_buffer_access_behavior");
    fprintf(stderr, "INFO version=%s robust_buffer_access=%d\n",
            glGetString(GL_VERSION), have);

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

    /* Two buffers allocated back to back, so that "past the end of the first"
     * is very likely to be "inside the second" for any driver that does not
     * bound the access. The sentinel is the whole test. */
    const unsigned words = 4;
    GLuint small_bo = 0, sentinel_bo = 0;
    uint32_t small_data[4] = {1, 2, 3, 4};
    uint32_t sentinel_data[64];
    for (unsigned i = 0; i < 64; ++i) sentinel_data[i] = SENTINEL;
    glGenBuffers(1, &small_bo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, small_bo);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(small_data), small_data,
                 GL_STATIC_DRAW);
    glGenBuffers(1, &sentinel_bo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, sentinel_bo);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(sentinel_data),
                 sentinel_data, GL_STATIC_DRAW);
    while (glGetError() != GL_NO_ERROR) {}

    /* Case 1: read far past the end of a storage buffer. The shader reports
     * whether what it read was the sentinel, which is the only wrong answer
     * -- zero and any of 1..4 are all permitted. */
    {
        static const char *fs =
            "#version 330 core\n"
            "#extension GL_ARB_shader_storage_buffer_object : require\n"
            "layout(std430, binding = 0) buffer Small { uint v[]; };\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  uint a = v[4096u];\n"
            "  uint b = v[65536u];\n"
            "  bool leaked = a == 0x5ca1ab1eu || b == 0x5ca1ab1eu;\n"
            "  colour = vec4(leaked ? 1.0 : 0.0, 0.0, 0.0, 1.0);\n"
            "}\n";
        GLuint prog = link_pair(quad_vs, fs);
        unsigned char px[4] = {0};
        if (prog) {
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, small_bo);
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, sentinel_bo);
            draw_and_read(prog, px);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO ssbo-read leaked=%u err=0x%04x\n", px[0],
                (unsigned)error);
        result("ssbo-read", have && prog && error == GL_NO_ERROR && px[0] == 0);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 2: write far past the end of it, then read the neighbour back on
     * the CPU. Nothing in it may have changed. */
    {
        static const char *fs =
            "#version 330 core\n"
            "#extension GL_ARB_shader_storage_buffer_object : require\n"
            "layout(std430, binding = 0) buffer Small { uint v[]; };\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  v[4096u] = 0xdeadbeefu;\n"
            "  v[65536u] = 0xdeadbeefu;\n"
            "  colour = vec4(0.0, 0.0, 0.0, 1.0);\n"
            "}\n";
        GLuint prog = link_pair(quad_vs, fs);
        unsigned char px[4] = {0};
        unsigned clobbered = 0;
        if (prog) {
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, small_bo);
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, sentinel_bo);
            draw_and_read(prog, px);
            glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, sentinel_bo);
            const uint32_t *read = (const uint32_t *)glMapBufferRange(
                GL_SHADER_STORAGE_BUFFER, 0, sizeof(sentinel_data),
                GL_MAP_READ_BIT);
            if (read) {
                for (unsigned i = 0; i < 64; ++i)
                    if (read[i] != SENTINEL) ++clobbered;
                glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
            } else {
                clobbered = 0xffff;
            }
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO ssbo-write clobbered=%u err=0x%04x\n", clobbered,
                (unsigned)error);
        result("ssbo-write",
               have && prog && error == GL_NO_ERROR && clobbered == 0);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 3: the same read through a uniform block, which is a different
     * binding type and a different bounds path. Indexed by a value the
     * compiler cannot fold, so the read cannot be turned into a constant. */
    {
        static const char *fs =
            "#version 330 core\n"
            "layout(std140) uniform Block { uvec4 v[4]; };\n"
            "uniform int far;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  uvec4 a = v[far];\n"
            "  bool leaked = a.x == 0x5ca1ab1eu || a.y == 0x5ca1ab1eu;\n"
            "  colour = vec4(leaked ? 1.0 : 0.0, 0.0, 0.0, 1.0);\n"
            "}\n";
        GLuint prog = link_pair(quad_vs, fs);
        unsigned char px[4] = {0};
        GLuint ubo = 0, ubo_sentinel = 0;
        if (prog) {
            uint32_t block[16] = {0};
            glGenBuffers(1, &ubo);
            glBindBuffer(GL_UNIFORM_BUFFER, ubo);
            glBufferData(GL_UNIFORM_BUFFER, sizeof(block), block,
                         GL_STATIC_DRAW);
            glGenBuffers(1, &ubo_sentinel);
            glBindBuffer(GL_UNIFORM_BUFFER, ubo_sentinel);
            glBufferData(GL_UNIFORM_BUFFER, sizeof(sentinel_data),
                         sentinel_data, GL_STATIC_DRAW);
            glBindBufferBase(GL_UNIFORM_BUFFER, 0, ubo);
            glBindBufferBase(GL_UNIFORM_BUFFER, 1, ubo_sentinel);
            const GLuint index = glGetUniformBlockIndex(prog, "Block");
            if (index != GL_INVALID_INDEX)
                glUniformBlockBinding(prog, index, 0);
            glUseProgram(prog);
            /* Past the declared array, which is the case the extension is
             * about: a std140 block is sized by its declaration and the
             * buffer behind it may be smaller. */
            glUniform1i(glGetUniformLocation(prog, "far"), 3);
            draw_and_read(prog, px);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO ubo-read leaked=%u err=0x%04x\n", px[0],
                (unsigned)error);
        result("ubo-read", have && prog && error == GL_NO_ERROR && px[0] == 0);
        if (prog) glDeleteProgram(prog);
        if (ubo) glDeleteBuffers(1, &ubo);
        if (ubo_sentinel) glDeleteBuffers(1, &ubo_sentinel);
    }

    /* Case 4: texelFetch past the end of a texture buffer. This driver serves
     * a samplerBuffer as a storage-buffer read, so the bound comes from the
     * binding rather than from a texture descriptor -- a different path from
     * case 1 despite looking the same in GLSL. */
    {
        static const char *fs =
            "#version 330 core\n"
            "uniform usamplerBuffer buf;\n"
            "uniform int far;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  uint a = texelFetch(buf, far).x;\n"
            "  colour = vec4(a == 0x5ca1ab1eu ? 1.0 : 0.0, 0.0, 0.0, 1.0);\n"
            "}\n";
        GLuint prog = link_pair(quad_vs, fs);
        unsigned char px[4] = {0};
        GLuint tex = 0, bo = 0, sentinel_tex = 0;
        if (prog) {
            uint32_t four[4] = {7, 8, 9, 10};
            glGenBuffers(1, &bo);
            glBindBuffer(GL_TEXTURE_BUFFER, bo);
            glBufferData(GL_TEXTURE_BUFFER, sizeof(four), four,
                         GL_STATIC_DRAW);
            glGenTextures(1, &tex);
            glBindTexture(GL_TEXTURE_BUFFER, tex);
            glTexBuffer(GL_TEXTURE_BUFFER, GL_R32UI, bo);
            /* A second texture buffer over the sentinel, bound to another
             * unit, so an escaping fetch has somewhere recognisable to land. */
            GLuint sentinel_buffer = 0;
            glGenBuffers(1, &sentinel_buffer);
            glBindBuffer(GL_TEXTURE_BUFFER, sentinel_buffer);
            glBufferData(GL_TEXTURE_BUFFER, sizeof(sentinel_data),
                         sentinel_data, GL_STATIC_DRAW);
            glGenTextures(1, &sentinel_tex);
            glActiveTexture(GL_TEXTURE1);
            glBindTexture(GL_TEXTURE_BUFFER, sentinel_tex);
            glTexBuffer(GL_TEXTURE_BUFFER, GL_R32UI, sentinel_buffer);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_BUFFER, tex);
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "buf"), 0);
            glUniform1i(glGetUniformLocation(prog, "far"), 4096);
            draw_and_read(prog, px);
            glDeleteBuffers(1, &sentinel_buffer);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO buftex-fetch leaked=%u err=0x%04x\n", px[0],
                (unsigned)error);
        result("buftex-fetch",
               have && prog && error == GL_NO_ERROR && px[0] == 0);
        if (prog) glDeleteProgram(prog);
        if (tex) glDeleteTextures(1, &tex);
        if (sentinel_tex) glDeleteTextures(1, &sentinel_tex);
        if (bo) glDeleteBuffers(1, &bo);
    }

    /* Case 5: the fixed-function half. An index list that runs past the end
     * of the vertex buffer must not fetch outside it, and -- whatever it
     * fetches -- must not take the context down. Checked by drawing after it
     * and requiring the later draw to work. */
    {
        static const char *vs =
            "#version 330 core\n"
            "in vec2 pos;\n"
            "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
        static const char *fs =
            "#version 330 core\n"
            "out vec4 colour;\n"
            "void main() { colour = vec4(0.0, 1.0, 0.0, 1.0); }\n";
        GLuint prog = link_pair(vs, fs);
        GLuint vbo = 0, ibo = 0;
        unsigned char after[4] = {0};
        int survived = 0;
        if (prog) {
            const float verts[8] = {
                -1.0f, -1.0f,  1.0f, -1.0f,  1.0f, 1.0f,  -1.0f, 1.0f,
            };
            /* Four vertices in the buffer, six indices reaching well past. */
            const uint32_t idx[6] = {0, 1, 2, 4000, 5000, 60000};
            glGenBuffers(1, &vbo);
            glBindBuffer(GL_ARRAY_BUFFER, vbo);
            glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts,
                         GL_STATIC_DRAW);
            glGenBuffers(1, &ibo);
            glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo);
            glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(idx), idx,
                         GL_STATIC_DRAW);
            glBindAttribLocation(prog, 0, "pos");
            glEnableVertexAttribArray(0);
            glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glDrawElements(GL_TRIANGLES, 6, GL_UNSIGNED_INT, NULL);
            while (glGetError() != GL_NO_ERROR) {}
            /* Now an ordinary draw. If the out-of-range one poisoned the
             * device, this one produces nothing. */
            glDisableVertexAttribArray(0);
            glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, 0);
            GLuint plain = link_pair(quad_vs, fs);
            if (plain) {
                draw_and_read(plain, after);
                survived = after[1] > 200;
                glDeleteProgram(plain);
            }
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO vertex-fetch survived=%d green=%u err=0x%04x\n",
                survived, after[1], (unsigned)error);
        result("vertex-fetch",
               have && prog && error == GL_NO_ERROR && survived);
        if (prog) glDeleteProgram(prog);
        if (vbo) glDeleteBuffers(1, &vbo);
        if (ibo) glDeleteBuffers(1, &ibo);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
