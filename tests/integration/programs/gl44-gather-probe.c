/* ARB_texture_gather: textureGather and textureGatherOffset.
 *
 * Gather returns the four texels a bilinear filter would blend, as one vec4,
 * which is how shadow filtering and most screen-space effects sample. WGSL has
 * textureGather and zink's SPIR-V emitter already emits nir_texop_tg4, so the
 * only thing between this and working is the cap st/mesa reads
 * (max_texture_gather_components).
 *
 * The texture is four texels with four distinct values, so a gather that
 * returns them in the wrong order -- the component order is
 * (i0j1, i1j1, i1j0, i0j0), counter-clockwise from the lower left, not raster
 * order -- fails rather than passing by luck.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

/* Gathers the red channel at the centre of a 2x2 texture, where the four
 * texels meet, and writes the four results as the four output channels. */
static const char *gather_source =
    "#version 330 core\n"
    "#extension GL_ARB_texture_gather : require\n"
    "uniform sampler2D tex;\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = textureGather(tex, vec2(0.5, 0.5)); }\n";

/* The component argument of textureGather needs ARB_gpu_shader5, which is a
 * GL 4.0 extension and not available here, so the second case reaches a
 * different channel by swizzling the view instead. That also exercises the
 * driver's own tg4 branch, which rewrites the gathered component rather than
 * shuffling the result -- a gather selects one channel, so its swizzle is a
 * component remap. */
static const char *gather_green_source = NULL;
static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
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

    /* The extension has to be there before the shader that requires it. */
    GLint gather_components = 0;
    glGetIntegerv(GL_MAX_PROGRAM_TEXTURE_GATHER_COMPONENTS_ARB,
                  &gather_components);
    const GLenum query_error = glGetError();
    fprintf(stderr, "INFO max_gather_components=%d query_err=0x%04x\n",
            gather_components, (unsigned)query_error);
    result("gather_advertised",
           gather_components >= 1 && query_error == GL_NO_ERROR);

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    if (!vertex) return 2;

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 2, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 2, 2);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    static const float positions[12] = {
        -1.0f, -1.0f,  1.0f, -1.0f,  -1.0f, 1.0f,
         1.0f, -1.0f,  1.0f,  1.0f,  -1.0f, 1.0f,
    };
    GLuint position_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(positions), positions, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    /* Four texels, red and green both distinct across all four. */
    static const unsigned char texels[16] = {
        10,  200, 0, 255,     /* (0,0) */
        70,  140, 0, 255,     /* (1,0) */
        130,  80, 0, 255,     /* (0,1) */
        190,  20, 0, 255,     /* (1,1) */
    };
    GLuint texture = 0;
    glGenTextures(1, &texture);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 2, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, texels);

    struct { const char *name; const char *source; unsigned char want[4]; } tests[] = {
        /* Gather order is (i0j1, i1j1, i1j0, i0j0): the lower-left texel last. */
        {"gather_red", gather_source, {130, 190, 70, 10}},
        /* Same shader, but the view's red is swizzled to green. */
        {"gather_swizzled_to_green", gather_source, {80, 20, 140, 200}},
    };

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_R,
                        i == 1 ? GL_GREEN : GL_RED);
        GLuint fs = compile(GL_FRAGMENT_SHADER, tests[i].source);
        if (!fs) { result(tests[i].name, 0); continue; }
        GLuint program = glCreateProgram();
        glAttachShader(program, vertex);
        glAttachShader(program, fs);
        glLinkProgram(program);
        GLint linked = 0;
        glGetProgramiv(program, GL_LINK_STATUS, &linked);
        if (!linked) {
            char log[512] = {0};
            glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s link failed: %s\n", tests[i].name, log);
            result(tests[i].name, 0);
            continue;
        }
        glUseProgram(program);
        glUniform1i(glGetUniformLocation(program, "tex"), 0);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 6);

        unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
        glReadPixels(1, 1, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
        const GLenum error = glGetError();
        int ok = error == GL_NO_ERROR;
        for (unsigned c = 0; c < 4; ++c) {
            const int delta = (int)got[c] - (int)tests[i].want[c];
            if (delta > 2 || delta < -2) ok = 0;
        }
        fprintf(stderr, "INFO %s got=%u,%u,%u,%u want=%u,%u,%u,%u err=0x%04x\n",
                tests[i].name, got[0], got[1], got[2], got[3],
                tests[i].want[0], tests[i].want[1], tests[i].want[2],
                tests[i].want[3], (unsigned)error);
        result(tests[i].name, ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
