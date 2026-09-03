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

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
