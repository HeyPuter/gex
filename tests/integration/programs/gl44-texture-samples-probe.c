/* ARB_shader_texture_image_samples: textureSamples() in a shader.
 *
 * The fourth of GL 4.5's five extensions. It reaches the emitter as
 * nir_texop_texture_samples and becomes OpImageQuerySamples, which Tint takes
 * as WGSL's textureNumSamples -- so, like the derivative-control pair, only
 * the cap was missing. This exists because a cap turned on without a test is
 * a claim rather than a capability.
 *
 * WebGPU core offers sample counts 1 and 4 and nothing else, so 4 is the
 * interesting answer and the probe asks the implementation what it actually
 * allocated rather than assuming.
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

#define SIDE 8

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
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
    const EGLint surface_attributes[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE,
                                         EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    const int have = have_extension("GL_ARB_shader_texture_image_samples");
    fprintf(stderr, "INFO version=%s texture_image_samples=%d\n",
            glGetString(GL_VERSION), have);
    if (!have) {
        fprintf(stderr, "INFO skipping: extension absent\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    /* The multisampled texture whose count the shader will report. */
    GLuint ms_texture = 0;
    glGenTextures(1, &ms_texture);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms_texture);
    glTexImage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, 4, GL_RGBA8, SIDE, SIDE,
                            GL_TRUE);
    GLint allocated = 0;
    glGetTexLevelParameteriv(GL_TEXTURE_2D_MULTISAMPLE, 0, GL_TEXTURE_SAMPLES,
                             &allocated);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO allocated samples=%d\n", allocated);

    /* A single-sampled target to write the answer into. */
    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "#extension GL_ARB_shader_texture_image_samples : require\n"
        "uniform sampler2DMS source;\n"
        "out vec4 colour;\n"
        "void main() {\n"
        "  colour = vec4(float(textureSamples(source)) / 8.0, 0.0, 0.0, 1.0);\n"
        "}\n";
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs_src, NULL);
    glCompileShader(v);
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs_src, NULL);
    glCompileShader(f);
    GLint ok = 0;
    glGetShaderiv(f, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetShaderInfoLog(f, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, v);
    glAttachShader(program, f);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
    }
    result("links", ok != 0);

    unsigned char px[4] = {0};
    GLenum error = GL_NO_ERROR;
    if (ok) {
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(program);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms_texture);
        glUniform1i(glGetUniformLocation(program, "source"), 0);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        error = glGetError();
    }
    /* What the implementation says it allocated, scaled the same way. */
    const unsigned want = (unsigned)((allocated / 8.0) * 255.0 + 0.5);
    fprintf(stderr, "INFO samples got=%u want=%u err=0x%04x\n", px[0], want,
            (unsigned)error);
    result("textureSamples",
           ok && error == GL_NO_ERROR && allocated > 0 &&
           px[0] + 2 >= want && px[0] <= want + 2);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
