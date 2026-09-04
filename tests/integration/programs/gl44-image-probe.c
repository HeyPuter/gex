/* Image load/store -- `ARB_shader_image_load_store`, GL 4.2, and the last
 * gate before `ARB_compute_shader`.
 *
 * st_extensions gates compute behind this and behind
 * `ARB_shader_atomic_counters`; the latter came free with shader storage
 * buffers, the compute limits are granted now (the adapter offers 1024
 * invocations per workgroup, it just had to be asked), and
 * `launch_grid`/`create_compute_state`/`set_shader_buffers` are already
 * implemented. So this is what stands between the driver and GL compute.
 *
 * WebGPU has storage textures, and this Dawn offers
 * `WGPUStorageTextureAccess_ReadOnly` and `_ReadWrite` with no feature gate --
 * but with a *format* restriction much tighter than GL's: read_write is
 * allowed on a narrow set, write-only on a wider one. Everything here uses
 * `r32ui`, which is in the narrow set, so a failure is about the mechanism
 * rather than about the format table.
 *
 *   extension_present  advertised, with fragment image units
 *   fragment_store     a fragment shader writes a texel, read back on the CPU
 *   fragment_load      a fragment shader reads a texel the CPU wrote
 *   image_size         imageSize() reports the real dimensions
 *
 * Integer images throughout: `r32ui` load and store are exact, so a wrong
 * answer is a wrong number rather than a rounding argument.
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

static GLuint build(const char *fs_src) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
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

static GLuint make_r32ui(unsigned fill) {
    GLuint tex = 0;
    unsigned *data = malloc((size_t)SIDE * SIDE * sizeof(unsigned));
    for (int i = 0; i < SIDE * SIDE; ++i) data[i] = fill;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, SIDE, SIDE, 0, GL_RED_INTEGER,
                 GL_UNSIGNED_INT, data);
    free(data);
    return tex;
}

static unsigned read_texel(GLuint tex, int x, int y) {
    unsigned *data = malloc((size_t)SIDE * SIDE * sizeof(unsigned));
    memset(data, 0xAB, (size_t)SIDE * SIDE * sizeof(unsigned));
    glBindTexture(GL_TEXTURE_2D, tex);
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RED_INTEGER, GL_UNSIGNED_INT, data);
    const unsigned v = data[(size_t)y * SIDE + x];
    free(data);
    return v;
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

    GLint frag_images = 0, image_units = 0;
    glGetIntegerv(GL_MAX_FRAGMENT_IMAGE_UNIFORMS, &frag_images);
    glGetIntegerv(GL_MAX_IMAGE_UNITS, &image_units);
    const int present = has_extension("GL_ARB_shader_image_load_store");
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO extension present=%d frag_images=%d units=%d "
            "compute=%d version=%s\n", present, frag_images, image_units,
            has_extension("GL_ARB_compute_shader"), glGetString(GL_VERSION));
    result("extension_present", present && frag_images > 0);
    if (!present) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    /* A tiny colour target, because the draw exists only to run the fragment
     * shader; the image is where the answer goes. */
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

    /* Every invocation writes the same value to the same texel, so the result
     * does not depend on how many fragments ran -- only on whether a store
     * arrived. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform uimage2D img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  imageStore(img, ivec2(2, 3), uvec4(4242u, 0u, 0u, 0u));\n"
            "  colour = vec4(0, 1, 0, 1);\n"
            "}\n");
        GLuint tex = make_r32ui(7u);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            /* The image unit comes from the API rather than from a
             * layout(binding=): that qualifier needs GLSL 420 or
             * ARB_shading_language_420pack, and this is a 3.3 context. */
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
        glMemoryBarrier(GL_ALL_BARRIER_BITS);
        const unsigned got = read_texel(tex, 2, 3);
        const unsigned untouched = read_texel(tex, 0, 0);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO fragment_store got=%u want=4242 untouched=%u "
                "want=7 err=0x%04x\n", got, untouched, (unsigned)error);
        result("fragment_store", prog && error == GL_NO_ERROR &&
                                 got == 4242u && untouched == 7u);
        glDeleteTextures(1, &tex);
    }

    /* Reading a texel the CPU wrote, carried out through the colour target so
     * the answer is visible without another readback path. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform uimage2D img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  uint v = imageLoad(img, ivec2(1, 1)).x;\n"
            "  colour = vec4(float(v) / 255.0, 0, 0, 1);\n"
            "}\n");
        GLuint tex = make_r32ui(96u);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_ONLY, GL_R32UI);
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            /* The image unit comes from the API rather than from a
             * layout(binding=): that qualifier needs GLSL 420 or
             * ARB_shading_language_420pack, and this is a 3.3 context. */
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO fragment_load got=%u want=96 err=0x%04x\n",
                px[0], (unsigned)error);
        result("fragment_load", prog && error == GL_NO_ERROR &&
                                px[0] >= 94 && px[0] <= 98);
        glDeleteTextures(1, &tex);
    }

    /* imageSize(), which is a different SPIR-V opcode from either access and
     * so a separate claim. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            /* imageSize() is ARB_shader_image_size (GL 4.3), a separate
             * extension from load/store -- st_extensions turns them on
             * together, but the shader still has to ask for both. */
            "#extension GL_ARB_shader_image_size : require\n"
            "layout(r32ui) uniform uimage2D img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  ivec2 s = imageSize(img);\n"
            "  colour = vec4(float(s.x) / 255.0, float(s.y) / 255.0, 0, 1);\n"
            "}\n");
        GLuint tex = make_r32ui(0u);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_ONLY, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            /* The image unit comes from the API rather than from a
             * layout(binding=): that qualifier needs GLSL 420 or
             * ARB_shading_language_420pack, and this is a 3.3 context. */
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO image_size got=%u,%u want=%u,%u err=0x%04x\n",
                px[0], px[1], (unsigned)SIDE, (unsigned)SIDE,
                (unsigned)error);
        result("image_size", prog && error == GL_NO_ERROR &&
                             px[0] == SIDE && px[1] == SIDE);
        glDeleteTextures(1, &tex);
    }

    /* `image2DRect`. SPIR-V has a Dim of its own for a rectangle image and
     * WGSL has no storage-texture type for it, so a shader declaring one was
     * refused outright -- and KHR-GL44.shader_image_load_store's allTargets
     * cases walk every target, so each of them stopped there.
     *
     * A rectangle image is a 2D image, though, and exactly so: the difference
     * between the two targets is that a rectangle texture is *sampled* with
     * unnormalised coordinates, and imageStore addresses by texel whatever
     * the dimension is. The store below therefore has to land at the texel it
     * names, which is what a driver that quietly substituted a 2D image with
     * scaled coordinates would get wrong. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform uimage2DRect img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  imageStore(img, ivec2(3, 2), uvec4(0xfeedu, 0, 0, 0));\n"
            "  colour = vec4(0, 1, 0, 1);\n"
            "}\n");
        GLuint tex = 0;
        unsigned *data = malloc((size_t)SIDE * SIDE * sizeof(unsigned));
        for (int i = 0; i < SIDE * SIDE; ++i) data[i] = 0u;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_RECTANGLE, tex);
        glTexParameteri(GL_TEXTURE_RECTANGLE, GL_TEXTURE_MIN_FILTER,
                        GL_NEAREST);
        glTexParameteri(GL_TEXTURE_RECTANGLE, GL_TEXTURE_MAG_FILTER,
                        GL_NEAREST);
        glTexImage2D(GL_TEXTURE_RECTANGLE, 0, GL_R32UI, SIDE, SIDE, 0,
                     GL_RED_INTEGER, GL_UNSIGNED_INT, data);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_WRITE_ONLY, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                            GL_TEXTURE_UPDATE_BARRIER_BIT);
        }
        memset(data, 0xAB, (size_t)SIDE * SIDE * sizeof(unsigned));
        glBindTexture(GL_TEXTURE_RECTANGLE, tex);
        glGetTexImage(GL_TEXTURE_RECTANGLE, 0, GL_RED_INTEGER,
                      GL_UNSIGNED_INT, data);
        const unsigned at = data[2 * SIDE + 3];
        /* A neighbour, so a driver that wrote the whole image fails too. */
        const unsigned next = data[2 * SIDE + 4];
        const GLenum error = glGetError();
        fprintf(stderr, "INFO rect_store at(3,2)=0x%x next=0x%x want=0xfeed,0 "
                "err=0x%04x\n", at, next, (unsigned)error);
        result("rect_store", prog && error == GL_NO_ERROR &&
                             at == 0xfeedu && next == 0u);
        free(data);
        glDeleteTextures(1, &tex);
    }

    /* `imageCube`. WGSL has no storage cube either, but a cube image is a 2D
     * array of six faces and GL's face order -- +X,-X,+Y,-Y,+Z,-Z -- is the
     * order a cube texture's array layers are in, so the two are the same
     * operation written twice. The store names face 3, GL_TEXTURE_CUBE_MAP_
     * NEGATIVE_Y, and the read back asks that face specifically: a driver
     * that dropped the layer, or counted the faces the other way round,
     * writes a different one and fails. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform uimageCube img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  imageStore(img, ivec3(1, 1, 3), uvec4(0xc0feu, 0, 0, 0));\n"
            "  colour = vec4(0, 1, 0, 1);\n"
            "}\n");
        GLuint tex = 0;
        unsigned *data = malloc((size_t)SIDE * SIDE * sizeof(unsigned));
        for (int i = 0; i < SIDE * SIDE; ++i) data[i] = 0u;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_CUBE_MAP, tex);
        glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MIN_FILTER,
                        GL_NEAREST);
        glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MAG_FILTER,
                        GL_NEAREST);
        for (int face = 0; face < 6; ++face)
            glTexImage2D(GL_TEXTURE_CUBE_MAP_POSITIVE_X + face, 0, GL_R32UI,
                         SIDE, SIDE, 0, GL_RED_INTEGER, GL_UNSIGNED_INT, data);
        /* Layered, so all six faces are one image. */
        glBindImageTexture(0, tex, 0, GL_TRUE, 0, GL_WRITE_ONLY, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                            GL_TEXTURE_UPDATE_BARRIER_BIT);
        }
        memset(data, 0xAB, (size_t)SIDE * SIDE * sizeof(unsigned));
        glBindTexture(GL_TEXTURE_CUBE_MAP, tex);
        glGetTexImage(GL_TEXTURE_CUBE_MAP_POSITIVE_X + 3, 0, GL_RED_INTEGER,
                      GL_UNSIGNED_INT, data);
        const unsigned written = data[1 * SIDE + 1];
        /* And face 0 must be untouched, so writing every face fails. */
        memset(data, 0xAB, (size_t)SIDE * SIDE * sizeof(unsigned));
        glGetTexImage(GL_TEXTURE_CUBE_MAP_POSITIVE_X, 0, GL_RED_INTEGER,
                      GL_UNSIGNED_INT, data);
        const unsigned other = data[1 * SIDE + 1];
        const GLenum error = glGetError();
        fprintf(stderr, "INFO cube_store face3=0x%x face0=0x%x want=0xc0fe,0 "
                "err=0x%04x\n", written, other, (unsigned)error);
        result("cube_store", prog && error == GL_NO_ERROR &&
                             written == 0xc0feu && other == 0u);
        free(data);
        glDeleteTextures(1, &tex);
    }

    /* `image1DArray`. WGSL has no storage 1d-array type, and a 1D array
     * texture is stored here as a 2D array one texel tall -- so `(x, layer)`
     * becomes `(x, 0, layer)` and the shapes match exactly. The store names
     * layer 2 and the read back asks layer 2, with layer 0 checked as well so
     * a driver that lost the layer index fails rather than passing on a write
     * that landed somewhere plausible.
     *
     * imageSize is checked in the same shader because it is the one place the
     * rewritten shape shows through: the query answers (width, 1, layers) and
     * the shader asked for (width, layers). */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "#extension GL_ARB_shader_image_size : require\n"
            "layout(r32ui) uniform uimage1DArray img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  ivec2 s = imageSize(img);\n"
            "  imageStore(img, ivec2(5, 2), uvec4(0xbeadu, 0, 0, 0));\n"
            "  colour = vec4(float(s.x) / 255.0, float(s.y) / 255.0, 0, 1);\n"
            "}\n");
        const int layers = 4;
        GLuint tex = 0;
        unsigned *data = malloc((size_t)SIDE * layers * sizeof(unsigned));
        for (int i = 0; i < SIDE * layers; ++i) data[i] = 0u;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_1D_ARRAY, tex);
        glTexParameteri(GL_TEXTURE_1D_ARRAY, GL_TEXTURE_MIN_FILTER,
                        GL_NEAREST);
        glTexParameteri(GL_TEXTURE_1D_ARRAY, GL_TEXTURE_MAG_FILTER,
                        GL_NEAREST);
        glTexImage2D(GL_TEXTURE_1D_ARRAY, 0, GL_R32UI, SIDE, layers, 0,
                     GL_RED_INTEGER, GL_UNSIGNED_INT, data);
        glBindImageTexture(0, tex, 0, GL_TRUE, 0, GL_WRITE_ONLY, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        unsigned char px[4] = {0};
        if (prog) {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                            GL_TEXTURE_UPDATE_BARRIER_BIT);
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
        }
        memset(data, 0xAB, (size_t)SIDE * layers * sizeof(unsigned));
        glBindTexture(GL_TEXTURE_1D_ARRAY, tex);
        glGetTexImage(GL_TEXTURE_1D_ARRAY, 0, GL_RED_INTEGER,
                      GL_UNSIGNED_INT, data);
        const unsigned written = data[2 * SIDE + 5];
        const unsigned other = data[0 * SIDE + 5];
        const GLenum error = glGetError();
        fprintf(stderr, "INFO array_store layer2=0x%x layer0=0x%x "
                "size=%u,%u want=0xbead,0 and %u,%d err=0x%04x\n", written,
                other, px[0], px[1], (unsigned)SIDE, layers,
                (unsigned)error);
        result("array_store", prog && error == GL_NO_ERROR &&
                              written == 0xbeadu && other == 0u);
        result("array_size", prog && error == GL_NO_ERROR &&
                             px[0] == SIDE && px[1] == layers);
        free(data);
        glDeleteTextures(1, &tex);
    }

    /* `image1D`. This one was never refused -- it was *silently wrong*, which
     * is worse: the driver asked for a 1D texture view, every non-3D texture
     * here is created as a 2D WebGPU texture, and an incompatible view is an
     * invalid object, so the bind group was rejected and the draw wrote
     * nothing at all with no GL error to show for it. A store that has to
     * land at texel 6 and nowhere else is what catches that. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform uimage1D img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  imageStore(img, 6, uvec4(0xd00du, 0, 0, 0));\n"
            "  colour = vec4(0, 1, 0, 1);\n"
            "}\n");
        GLuint tex = 0;
        unsigned *data = malloc((size_t)SIDE * sizeof(unsigned));
        for (int i = 0; i < SIDE; ++i) data[i] = 0u;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_1D, tex);
        glTexParameteri(GL_TEXTURE_1D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_1D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexImage1D(GL_TEXTURE_1D, 0, GL_R32UI, SIDE, 0, GL_RED_INTEGER,
                     GL_UNSIGNED_INT, data);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_WRITE_ONLY, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                            GL_TEXTURE_UPDATE_BARRIER_BIT);
        }
        memset(data, 0xAB, (size_t)SIDE * sizeof(unsigned));
        glBindTexture(GL_TEXTURE_1D, tex);
        glGetTexImage(GL_TEXTURE_1D, 0, GL_RED_INTEGER, GL_UNSIGNED_INT,
                      data);
        const unsigned at = data[6];
        const unsigned next = data[5];
        const GLenum error = glGetError();
        fprintf(stderr, "INFO one_d_store at6=0x%x at5=0x%x want=0xd00d,0 "
                "err=0x%04x\n", at, next, (unsigned)error);
        result("one_d_store", prog && error == GL_NO_ERROR &&
                              at == 0xd00du && next == 0u);
        free(data);
        glDeleteTextures(1, &tex);
    }

    /* `imageBuffer`. WebGPU has no texel buffer at all, so there is nothing
     * to compare a format against -- but there is also nothing to copy: the
     * resource *is* a buffer, so the shader packs into it in place and this
     * is the one image shape that costs nothing to serve. The read-only half
     * of the same idea is `samplerBuffer`, which has worked for a while.
     *
     * The store names texel 4 and its neighbours are checked, because the
     * texel address here is arithmetic the driver does itself rather than
     * something the hardware indexes. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform uimageBuffer img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  imageStore(img, 4, uvec4(0xfaceu, 0, 0, 0));\n"
            "  colour = vec4(0, 1, 0, 1);\n"
            "}\n");
        const int texels = 8;
        /* Seeded with a pattern rather than zeroes, and that mattered: with
         * a zero seed a store that never happened and a store that wrote
         * zero read back identically, which is exactly the ambiguity that
         * hid a bug here -- every buffer image was taking the driver's
         * incomplete-texture path, because that check asks whether the
         * resource has a *texture* and a buffer resource has none. The draw
         * ran, the module was right, the stores went to a scratch buffer and
         * were discarded, and nothing anywhere said so. */
        unsigned seed[8];
        for (int i = 0; i < 8; ++i) seed[i] = 0x11111111u;
        GLuint buf = 0, tex = 0;
        glGenBuffers(1, &buf);
        glBindBuffer(GL_TEXTURE_BUFFER, buf);
        glBufferData(GL_TEXTURE_BUFFER, sizeof(seed), seed, GL_DYNAMIC_DRAW);
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_BUFFER, tex);
        glTexBuffer(GL_TEXTURE_BUFFER, GL_R32UI, buf);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_WRITE_ONLY, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                            GL_BUFFER_UPDATE_BARRIER_BIT);
        }
        unsigned back[8];
        memset(back, 0xAB, sizeof(back));
        glBindBuffer(GL_TEXTURE_BUFFER, buf);
        glGetBufferSubData(GL_TEXTURE_BUFFER, 0, sizeof(back), back);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO buffer_store at4=0x%x at3=0x%x at5=0x%x "
                "want=0xface and 0x11111111 twice err=0x%04x\n", back[4],
                back[3], back[5], (unsigned)error);
        (void)texels;
        result("buffer_store", prog && error == GL_NO_ERROR &&
                               back[4] == 0xfaceu &&
                               back[3] == 0x11111111u &&
                               back[5] == 0x11111111u);
        glDeleteTextures(1, &tex);
        glDeleteBuffers(1, &buf);
    }

    /* A *read-only* buffer image. This is not the same claim as the store
     * above: a read-only *texture* image falls back to being sampled, whose
     * format table is far richer, but a buffer image has no such fallback --
     * WebGPU has no texel buffer at all -- so it has to take the same path
     * whichever way it is accessed. Reading a texel the CPU wrote is what
     * says the address arithmetic agrees in both directions. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform readonly uimageBuffer img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  uint v = imageLoad(img, 3).x;\n"
            "  colour = vec4(float(v) / 255.0, 0, 0, 1);\n"
            "}\n");
        unsigned seed[8];
        for (int i = 0; i < 8; ++i) seed[i] = 0u;
        seed[3] = 200u;
        GLuint buf = 0, tex = 0;
        glGenBuffers(1, &buf);
        glBindBuffer(GL_TEXTURE_BUFFER, buf);
        glBufferData(GL_TEXTURE_BUFFER, sizeof(seed), seed, GL_DYNAMIC_DRAW);
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_BUFFER, tex);
        glTexBuffer(GL_TEXTURE_BUFFER, GL_R32UI, buf);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_ONLY, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        unsigned char px[4] = {0};
        if (prog) {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO buffer_load got=%u want=200 err=0x%04x\n",
                px[0], (unsigned)error);
        result("buffer_load", prog && error == GL_NO_ERROR && px[0] == 200);
        glDeleteTextures(1, &tex);
        glDeleteBuffers(1, &buf);
    }

    /* Image atomics. GL's imageAtomic* reach SPIR-V as OpImageTexelPointer,
     * whose result lives in storage class Image, and Tint's reader has no
     * such class -- "unknown SPIR-V storage class: 11". So the atomic decides
     * the *binding*: such an image is served through a buffer, where a texel
     * that is a whole 32-bit word makes the image atomic an ordinary buffer
     * atomic. This was Blender EEVEE's last shader-level refusal.
     *
     * Every fragment adds 1 to the same texel, so the answer is the number of
     * fragments -- SIDE * SIDE -- and any implementation that lost the
     * atomicity, or turned the add into a store, gets a smaller number. A
     * second texel is left alone to catch an address that ignored the
     * coordinate. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform uimage2D img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  imageAtomicAdd(img, ivec2(2, 3), 1u);\n"
            "  colour = vec4(0, 1, 0, 1);\n"
            "}\n");
        GLuint tex = make_r32ui(7u);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                            GL_TEXTURE_UPDATE_BARRIER_BIT);
        }
        const unsigned counted = read_texel(tex, 2, 3);
        const unsigned untouched = read_texel(tex, 3, 3);
        const GLenum error = glGetError();
        /* Seeded at 7, so the total is 7 + one per fragment. */
        const unsigned want = 7u + (unsigned)(SIDE * SIDE);
        fprintf(stderr, "INFO atomic_add got=%u want=%u other=%u want=7 "
                "err=0x%04x\n", counted, want, untouched, (unsigned)error);
        result("atomic_add", prog && error == GL_NO_ERROR &&
                             counted == want && untouched == 7u);
        glDeleteTextures(1, &tex);
    }

    /* imageAtomicCompSwap, which takes an extra operand and so travels a
     * different source layout -- getting that wrong reads the comparand as
     * the value and swaps in the wrong number. */
    {
        GLuint prog = build(
            "#version 330 core\n"
            "#extension GL_ARB_shader_image_load_store : require\n"
            "layout(r32ui) uniform uimage2D img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  imageAtomicCompSwap(img, ivec2(1, 1), 9u, 0xabcu);\n"
            "  colour = vec4(0, 1, 0, 1);\n"
            "}\n");
        GLuint tex = make_r32ui(9u);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32UI);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "img"), 0);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                            GL_TEXTURE_UPDATE_BARRIER_BIT);
        }
        const unsigned swapped = read_texel(tex, 1, 1);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO atomic_swap got=0x%x want=0xabc err=0x%04x\n",
                swapped, (unsigned)error);
        result("atomic_swap", prog && error == GL_NO_ERROR &&
                              swapped == 0xabcu);
        glDeleteTextures(1, &tex);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
