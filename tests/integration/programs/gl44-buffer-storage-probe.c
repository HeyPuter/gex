/* ARB_buffer_storage -- the last of GL 4.4's five.
 *
 * glBufferStorage allocates a buffer whose *size and flags* are fixed for
 * life, and its point is the flags: GL_MAP_PERSISTENT_BIT keeps a mapping
 * valid across draws, and GL_MAP_COHERENT_BIT makes writes through that
 * mapping visible to the GPU with no flush and no unmap. Together they are how
 * a modern renderer streams: map once at startup, write vertices or uniforms
 * straight into the mapping every frame, never call into the driver to hand
 * them over.
 *
 * WebGPU has no persistent mapping at all -- a mapped GPUBuffer is
 * unusable by the GPU until it is unmapped. So the only way to serve this is
 * for the driver to hold a CPU copy and hand the GPU the bytes itself, which
 * makes "coherent" a promise the driver keeps rather than one the hardware
 * does. Whether it keeps it is exactly what the cases below ask:
 *
 *   `storage`      glBufferStorage allocates, and the flags read back
 *   `immutable`    glBufferData on it is GL_INVALID_OPERATION, which is what
 *                  "immutable" means and the one thing a driver ignoring the
 *                  extension gets wrong for free
 *   `mapped-draw`  vertices written through a PERSISTENT|COHERENT mapping are
 *                  drawn -- with no flush, no unmap and no barrier
 *   `second-draw`  *different* vertices written through the same mapping,
 *                  still without unmapping, are drawn instead. This is the
 *                  case that separates a real implementation from one that
 *                  uploads at map time and never again
 *   `read`         the mapping reads back what was written through it
 *
 * The geometry is chosen so that "it drew" and "it drew the new thing" are
 * different answers: each set of vertices covers one half of the viewport, so
 * a stale upload leaves the wrong half painted.
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

#define SIDE 16

static GLuint prog;

/* A quad covering the left or the right half of clip space. */
static void fill_half(float *out, int right)
{
    const float x0 = right ? 0.0f : -1.0f;
    const float x1 = right ? 1.0f : 0.0f;
    const float verts[8] = {x0, -1.0f, x1, -1.0f, x0, 1.0f, x1, 1.0f};
    memcpy(out, verts, sizeof(verts));
}

/* Covered pixels in the left and the right half. */
static void coverage(unsigned *left, unsigned *right)
{
    unsigned char pixels[SIDE * SIDE * 4];
    glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    *left = *right = 0;
    for (int y = 0; y < SIDE; ++y)
        for (int x = 0; x < SIDE; ++x)
            if (pixels[(y * SIDE + x) * 4] > 200) {
                if (x < SIDE / 2) ++*left; else ++*right;
            }
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

    const int have = have_extension("GL_ARB_buffer_storage");
    fprintf(stderr, "INFO version=%s buffer_storage=%d\n",
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
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);

    static const char *vs_src =
        "#version 330 core\n"
        "in vec2 pos;\n"
        "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
    static const char *fs_src =
        "#version 330 core\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(1.0); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glBindAttribLocation(prog, 0, "pos");
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) return 2;

    /* Case 1: allocate, and the flags survive. */
    GLuint vbo = 0;
    GLint flags = 0;
    const GLbitfield want_flags = GL_MAP_READ_BIT | GL_MAP_WRITE_BIT |
                                  GL_MAP_PERSISTENT_BIT |
                                  GL_MAP_COHERENT_BIT;
    if (have) {
        glGenBuffers(1, &vbo);
        glBindBuffer(GL_ARRAY_BUFFER, vbo);
        glBufferStorage(GL_ARRAY_BUFFER, 8 * sizeof(float), NULL, want_flags);
        glGetBufferParameteriv(GL_ARRAY_BUFFER, GL_BUFFER_STORAGE_FLAGS,
                               &flags);
    }
    {
        const GLenum error = glGetError();
        fprintf(stderr, "INFO storage flags=0x%x want=0x%x err=0x%04x\n",
                (unsigned)flags, (unsigned)want_flags, (unsigned)error);
        result("storage", have && error == GL_NO_ERROR &&
                          (GLbitfield)flags == want_flags);
    }

    /* Case 2: it is immutable. */
    {
        GLenum error = GL_NO_ERROR;
        if (have) {
            while (glGetError() != GL_NO_ERROR) {}
            glBufferData(GL_ARRAY_BUFFER, 8 * sizeof(float), NULL,
                         GL_STATIC_DRAW);
            error = glGetError();
        }
        fprintf(stderr, "INFO immutable err=0x%04x want=0x0502\n",
                (unsigned)error);
        result("immutable", have && error == GL_INVALID_OPERATION);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* Cases 3 and 4: one mapping, two different contents, no unmap between. */
    float *mapped = NULL;
    if (have)
        mapped = (float *)glMapBufferRange(GL_ARRAY_BUFFER, 0,
                                           8 * sizeof(float), want_flags);
    {
        unsigned left = 0, right = 0;
        if (mapped) {
            fill_half(mapped, 0);
            glEnableVertexAttribArray(0);
            glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            coverage(&left, &right);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO mapped-draw left=%u right=%u want=%u,0 "
                "err=0x%04x\n", left, right, SIDE * SIDE / 2,
                (unsigned)error);
        result("mapped-draw", have && mapped && error == GL_NO_ERROR &&
                              left == (unsigned)(SIDE * SIDE / 2) &&
                              right == 0);
    }
    {
        unsigned left = 0, right = 0;
        if (mapped) {
            /* The same mapping, the other half, and nothing in between: no
             * glFlushMappedBufferRange, no glUnmapBuffer, no barrier. */
            fill_half(mapped, 1);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            coverage(&left, &right);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO second-draw left=%u right=%u want=0,%u "
                "err=0x%04x\n", left, right, SIDE * SIDE / 2,
                (unsigned)error);
        result("second-draw", have && mapped && error == GL_NO_ERROR &&
                              left == 0 &&
                              right == (unsigned)(SIDE * SIDE / 2));
    }

    /* Case 5: the mapping reads back what went through it. */
    {
        int same = 0;
        if (mapped) {
            float want[8];
            fill_half(want, 1);
            same = memcmp(mapped, want, sizeof(want)) == 0;
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO read same=%d err=0x%04x\n", same,
                (unsigned)error);
        result("read", have && mapped && error == GL_NO_ERROR && same);
    }
    if (mapped) {
        glUnmapBuffer(GL_ARRAY_BUFFER);
        glDisableVertexAttribArray(0);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
