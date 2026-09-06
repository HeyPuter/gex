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

    /* Case 1b: the same answer through the three DSA spellings.  The bound
     * form is answered from a guest-side record -- the guest ORs
     * GL_DYNAMIC_STORAGE_BIT into what it forwards so its mapping emulation's
     * glBufferSubData is accepted -- and the named spellings originally
     * forwarded to the host and reported that extra bit back. */
    {
        GLint named = 0;
        GLint64 named64 = 0;
        GLenum error = GL_NO_ERROR;
        if (have) {
            while (glGetError() != GL_NO_ERROR) {}
            glGetNamedBufferParameteriv(vbo, GL_BUFFER_STORAGE_FLAGS, &named);
            glGetNamedBufferParameteri64v(vbo, GL_BUFFER_STORAGE_FLAGS,
                                          &named64);
            error = glGetError();
        }
        fprintf(stderr, "INFO named flags=0x%x named64=0x%llx want=0x%x "
                "err=0x%04x\n", (unsigned)named,
                (unsigned long long)named64, (unsigned)want_flags,
                (unsigned)error);
        result("storage_named", have && error == GL_NO_ERROR &&
                                (GLbitfield)named == want_flags);
        result("storage_named64", have && error == GL_NO_ERROR &&
                                  (GLbitfield)named64 == want_flags);
    }

    /* Case 1c: a buffer whose storage was allocated through DSA, never bound.
     * glBufferStorage records under the bound name; glNamedBufferStorage has
     * to record under the name it was handed. */
    {
        GLuint dsa = 0;
        GLint dsa_flags = 0;
        GLenum error = GL_NO_ERROR;
        if (have) {
            while (glGetError() != GL_NO_ERROR) {}
            glCreateBuffers(1, &dsa);
            glNamedBufferStorage(dsa, 8 * sizeof(float), NULL, want_flags);
            glGetNamedBufferParameteriv(dsa, GL_BUFFER_STORAGE_FLAGS,
                                        &dsa_flags);
            error = glGetError();
        }
        fprintf(stderr, "INFO dsa flags=0x%x want=0x%x err=0x%04x\n",
                (unsigned)dsa_flags, (unsigned)want_flags, (unsigned)error);
        result("storage_dsa", have && error == GL_NO_ERROR &&
                              (GLbitfield)dsa_flags == want_flags);
        if (dsa) glDeleteBuffers(1, &dsa);
    }

    /* Case 1d: allocated on a target reached only through an indexed bind.
     * glBindBufferBase is defined to set the generic binding point too, and
     * the guest keys its buffer state off that point -- so when it did not,
     * glBufferStorage here recorded nothing, GL_BUFFER_STORAGE_FLAGS read
     * back 0, and glMapBufferRange on the target returned NULL.  This is what
     * KHR-GL44.buffer_storage's atomic-counter cases exercise. */
    {
        GLuint acb = 0;
        GLint acb_flags = 0;
        void *acb_map = NULL;
        GLenum error = GL_NO_ERROR;
        if (have) {
            while (glGetError() != GL_NO_ERROR) {}
            glGenBuffers(1, &acb);
            glBindBufferBase(GL_ATOMIC_COUNTER_BUFFER, 0, acb);
            glBufferStorage(GL_ATOMIC_COUNTER_BUFFER, 8 * sizeof(float), NULL,
                            want_flags);
            glGetBufferParameteriv(GL_ATOMIC_COUNTER_BUFFER,
                                   GL_BUFFER_STORAGE_FLAGS, &acb_flags);
            acb_map = glMapBufferRange(GL_ATOMIC_COUNTER_BUFFER, 0,
                                       8 * sizeof(float), want_flags);
            error = glGetError();
        }
        fprintf(stderr, "INFO indexed flags=0x%x want=0x%x map=%d err=0x%04x\n",
                (unsigned)acb_flags, (unsigned)want_flags, acb_map != NULL,
                (unsigned)error);
        result("storage_indexed", have && error == GL_NO_ERROR &&
                                  (GLbitfield)acb_flags == want_flags);
        result("map_indexed", have && acb_map != NULL);
        if (acb_map) glUnmapBuffer(GL_ATOMIC_COUNTER_BUFFER);
        if (acb) glDeleteBuffers(1, &acb);
        glBindBufferBase(GL_ATOMIC_COUNTER_BUFFER, 0, 0);
        while (glGetError() != GL_NO_ERROR) {}
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

    /* Cases 6-9: mapping's own observable surface.  The guest emulates
     * mapping with a shadow allocation and never tells the host a buffer is
     * mapped, so the host answers these queries for a buffer it believes is
     * idle and raises none of mapping's errors.  Both belong on the guest
     * side, with the bookkeeping. */
    {
        GLuint q = 0;
        GLint qmapped = -1, qaccess = -1, qoff = -1, qlen = -1, qlegacy = -1;
        void *m = NULL;
        GLenum error = GL_NO_ERROR;
        if (have) {
            while (glGetError() != GL_NO_ERROR) {}
            glGenBuffers(1, &q);
            glBindBuffer(GL_ARRAY_BUFFER, q);
            glBufferStorage(GL_ARRAY_BUFFER, 64, NULL,
                            GL_MAP_READ_BIT | GL_MAP_WRITE_BIT |
                            GL_DYNAMIC_STORAGE_BIT);
            m = glMapBufferRange(GL_ARRAY_BUFFER, 16, 32, GL_MAP_WRITE_BIT);
            glGetBufferParameteriv(GL_ARRAY_BUFFER, GL_BUFFER_MAPPED, &qmapped);
            glGetBufferParameteriv(GL_ARRAY_BUFFER, GL_BUFFER_ACCESS_FLAGS,
                                   &qaccess);
            glGetBufferParameteriv(GL_ARRAY_BUFFER, GL_BUFFER_MAP_OFFSET, &qoff);
            glGetBufferParameteriv(GL_ARRAY_BUFFER, GL_BUFFER_MAP_LENGTH, &qlen);
            glGetBufferParameteriv(GL_ARRAY_BUFFER, GL_BUFFER_ACCESS, &qlegacy);
            error = glGetError();
        }
        fprintf(stderr, "INFO mapstate mapped=%d access=0x%x off=%d len=%d "
                "legacy=0x%x err=0x%04x\n", qmapped, (unsigned)qaccess, qoff,
                qlen, (unsigned)qlegacy, (unsigned)error);
        result("map_state", have && m && error == GL_NO_ERROR &&
                            qmapped == GL_TRUE &&
                            (GLbitfield)qaccess == GL_MAP_WRITE_BIT &&
                            qoff == 16 && qlen == 32 &&
                            qlegacy == GL_WRITE_ONLY);

        /* A write into a range the non-persistent mapping covers is an error
         * -- GL 4.4 section 6.2.  The overlap is 16..32 against 24..40. */
        GLenum overlap = GL_NO_ERROR, apart = GL_NO_ERROR;
        if (have && m) {
            static const unsigned char bytes[16] = {0};
            while (glGetError() != GL_NO_ERROR) {}
            glBufferSubData(GL_ARRAY_BUFFER, 24, 16, bytes);
            overlap = glGetError();
            glBufferSubData(GL_ARRAY_BUFFER, 48, 16, bytes);
            apart = glGetError();
        }
        fprintf(stderr, "INFO subdata overlap=0x%04x want=0x0502 "
                "apart=0x%04x want=0x0000\n", (unsigned)overlap,
                (unsigned)apart);
        result("subdata_while_mapped", have && m &&
                                       overlap == GL_INVALID_OPERATION &&
                                       apart == GL_NO_ERROR);
        if (m) glUnmapBuffer(GL_ARRAY_BUFFER);
        if (q) glDeleteBuffers(1, &q);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* A negative size is GL_INVALID_VALUE, not a silent refusal: an
     * application that asks glGetError is told nothing went wrong and then
     * uses a buffer that has no storage. */
    {
        GLuint n = 0;
        GLenum bound_err = GL_NO_ERROR, named_err = GL_NO_ERROR;
        if (have) {
            glGenBuffers(1, &n);
            glBindBuffer(GL_ARRAY_BUFFER, n);
            while (glGetError() != GL_NO_ERROR) {}
            glBufferStorage(GL_ARRAY_BUFFER, -16, NULL, GL_MAP_READ_BIT);
            bound_err = glGetError();
            glNamedBufferStorage(n, -16, NULL, GL_MAP_READ_BIT);
            named_err = glGetError();
        }
        fprintf(stderr, "INFO negsize bound=0x%04x named=0x%04x want=0x0501\n",
                (unsigned)bound_err, (unsigned)named_err);
        result("negative_size", have && bound_err == GL_INVALID_VALUE &&
                                named_err == GL_INVALID_VALUE);
        if (n) glDeleteBuffers(1, &n);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* Without GL_DYNAMIC_STORAGE_BIT an immutable buffer cannot be written
     * with glBufferSubData at all.  The host's copy of the storage flags
     * always has that bit -- it is what makes the mapping write-back work --
     * so the host can never raise this and the guest must. */
    {
        GLuint s2 = 0;
        GLenum error = GL_NO_ERROR;
        if (have) {
            static const unsigned char bytes[16] = {0};
            glGenBuffers(1, &s2);
            glBindBuffer(GL_ARRAY_BUFFER, s2);
            glBufferStorage(GL_ARRAY_BUFFER, 64, NULL, GL_MAP_READ_BIT);
            while (glGetError() != GL_NO_ERROR) {}
            glBufferSubData(GL_ARRAY_BUFFER, 0, 16, bytes);
            error = glGetError();
        }
        fprintf(stderr, "INFO nodynamic err=0x%04x want=0x0502\n",
                (unsigned)error);
        result("subdata_not_dynamic", have && error == GL_INVALID_OPERATION);
        if (s2) glDeleteBuffers(1, &s2);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* An immutable buffer may only be mapped with access its storage allows.
     * The host's copy of the storage flags carries an extra
     * GL_DYNAMIC_STORAGE_BIT so a write-back is accepted at all, so only the
     * guest can make this check. */
    {
        GLuint d = 0;
        void *bad = (void *)-1;
        GLenum error = GL_NO_ERROR;
        if (have) {
            glGenBuffers(1, &d);
            glBindBuffer(GL_ARRAY_BUFFER, d);
            glBufferStorage(GL_ARRAY_BUFFER, 64, NULL, GL_DYNAMIC_STORAGE_BIT);
            while (glGetError() != GL_NO_ERROR) {}
            bad = glMapBufferRange(GL_ARRAY_BUFFER, 0, 64, GL_MAP_READ_BIT);
            error = glGetError();
        }
        fprintf(stderr, "INFO badaccess map=%d err=0x%04x want=0x0502\n",
                bad != NULL, (unsigned)error);
        result("map_access_gated", have && bad == NULL &&
                                   error == GL_INVALID_OPERATION);
        if (d) glDeleteBuffers(1, &d);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* A persistent mapping is two-way.  The guest fills its shadow once, at
     * map time; without a refresh at the point GL says the GPU's writes
     * become visible, everything reading GPU output through a persistent
     * mapping -- an asynchronous PBO read-back, a compute result -- sees
     * whatever was there when it mapped. */
    {
        GLuint pack = 0, tex = 0, fbo = 0;
        unsigned char *pm = NULL;
        int correct = 0;
        GLenum mapped_err = GL_NO_ERROR;
        if (have) {
            while (glGetError() != GL_NO_ERROR) {}
            glGenTextures(1, &tex);
            glBindTexture(GL_TEXTURE_2D, tex);
            glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 4, 4);
            glGenFramebuffers(1, &fbo);
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                   GL_TEXTURE_2D, tex, 0);
            glViewport(0, 0, 4, 4);
            glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);

            glGenBuffers(1, &pack);
            glBindBuffer(GL_PIXEL_PACK_BUFFER, pack);
            glBufferStorage(GL_PIXEL_PACK_BUFFER, 4 * 4 * 4, NULL,
                            GL_MAP_READ_BIT | GL_MAP_PERSISTENT_BIT |
                            GL_MAP_COHERENT_BIT);
            pm = (unsigned char *)glMapBufferRange(
                GL_PIXEL_PACK_BUFFER, 0, 4 * 4 * 4,
                GL_MAP_READ_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_COHERENT_BIT);
            glReadPixels(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
            glMemoryBarrier(GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT);
            glFinish();
            if (pm) correct = pm[0] == 0xff && pm[1] == 0x00 &&
                              pm[2] == 0x00 && pm[3] == 0xff;
        }
        fprintf(stderr, "INFO persistread map=%d first=%02x%02x%02x%02x "
                "want=ff0000ff\n", pm != NULL,
                pm ? pm[0] : 0, pm ? pm[1] : 0, pm ? pm[2] : 0, pm ? pm[3] : 0);
        result("persistent_readback", have && pm && correct);
        if (pm) glUnmapBuffer(GL_PIXEL_PACK_BUFFER);

        /* And packing into a mapping that is *not* persistent is an error. */
        if (have) {
            while (glGetError() != GL_NO_ERROR) {}
            void *np = glMapBufferRange(GL_PIXEL_PACK_BUFFER, 0, 4 * 4 * 4,
                                        GL_MAP_READ_BIT);
            if (np) {
                glReadPixels(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
                mapped_err = glGetError();
                glUnmapBuffer(GL_PIXEL_PACK_BUFFER);
            }
        }
        fprintf(stderr, "INFO packmapped err=0x%04x want=0x0502\n",
                (unsigned)mapped_err);
        result("readpixels_into_mapped", have &&
                                         mapped_err == GL_INVALID_OPERATION);
        glBindBuffer(GL_PIXEL_PACK_BUFFER, 0);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        if (pack) glDeleteBuffers(1, &pack);
        if (fbo) glDeleteFramebuffers(1, &fbo);
        if (tex) glDeleteTextures(1, &tex);
        while (glGetError() != GL_NO_ERROR) {}
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
