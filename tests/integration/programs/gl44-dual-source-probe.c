/* Dual-source blending -- `ARB_blend_func_extended`, core in GL 3.3.
 *
 * A fragment shader writes *two* colours to draw buffer zero, at index 0 and
 * index 1, and the blend equation may name the second one as a factor. It is
 * how a subpixel-antialiased glyph is composited, and Blender's EEVEE uses it
 * for its light-culling debug pass.
 *
 * WebGPU has the feature and this Dawn grants it, and the driver had already
 * allowed the four SRC1 blend factors once it was granted -- so the blend
 * state was right and the *shader* was rejected:
 *
 *   '@blend_src' and '@location' are used on one member while another member
 *   with '@location' doesn't use '@blend_src'
 *
 * WGSL allows `@blend_src` only on a pair, so a lone output must not carry
 * one -- which is why the SPIR-V emitter omits the Index decoration on index
 * 0 -- but a shader that *does* have an index 1 output then has @blend_src on
 * one member and not the other. Both or neither; nothing in between.
 *
 *   `extension`   the extension is advertised with at least one dual-source
 *                 draw buffer
 *   `links`       a shader declaring both indices links at all, which is what
 *                 the rejection above prevented
 *   `src1_color`  GL_SRC1_COLOR multiplies the destination by the *second*
 *                 output, not the first and not its alpha
 *   `src1_alpha`  GL_SRC1_ALPHA uses that output's alpha alone
 *
 * The two blending cases are what separate a driver that linked the shader
 * from one that also wired the second output up: the first output is zero
 * throughout, so anything that read it instead produces black.
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

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const GLubyte *one = glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (one && strcmp((const char *)one, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 8

/* colour0 is black, colour1 is the grey the blend factor reads. Keeping the
 * first output at zero is deliberate: with GL_ONE for the source factor the
 * result depends only on the destination and the *second* output, so a driver
 * that fed index 0 into the factor produces black and fails. */
static GLuint build(void) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "layout(location = 0, index = 0) out vec4 colour0;\n"
        "layout(location = 0, index = 1) out vec4 colour1;\n"
        "void main() {\n"
        "  colour0 = vec4(0.0, 0.0, 0.0, 0.0);\n"
        "  colour1 = vec4(0.5, 0.5, 0.5, 0.25);\n"
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
        char log[900] = {0};
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
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return prog;
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have = have_extension("GL_ARB_blend_func_extended");
    GLint buffers = 0;
    glGetIntegerv(GL_MAX_DUAL_SOURCE_DRAW_BUFFERS, &buffers);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s ext=%d dual_source_buffers=%d\n",
            glGetString(GL_VERSION), have, buffers);
    result("extension", have && buffers >= 1);

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

    GLuint program = build();
    result("links", program != 0);
    if (!program) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
    glUseProgram(program);

    /* dst = src0 * ONE + dst * <factor>, and src0 is zero, so the answer is
     * the destination scaled by whichever part of the second output the
     * factor names. Destination 0.8 -> 204. */
    struct { const char *name; GLenum factor; unsigned want; } tests[] = {
        /* 204 * 0.5 = 102 */
        {"src1_color", GL_SRC1_COLOR, 102},
        /* 204 * 0.25 = 51 */
        {"src1_alpha", GL_SRC1_ALPHA, 51},
    };
    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        glClearColor(0.8f, 0.8f, 0.8f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glEnable(GL_BLEND);
        glBlendFunc(GL_ONE, tests[i].factor);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glDisable(GL_BLEND);
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO %s got=%u,%u,%u want=%u err=0x%04x\n",
                tests[i].name, px[0], px[1], px[2], tests[i].want,
                (unsigned)error);
        const int ok = error == GL_NO_ERROR &&
                       px[0] + 3 >= tests[i].want && px[0] <= tests[i].want + 3;
        result(tests[i].name, ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
