/* ARB_derivative_control: dFdxFine/dFdxCoarse and their y counterparts.
 *
 * One of the five extensions GL 4.5 wants. The whole chain already existed --
 * zink's emitter has SpvOpDPdxFine/Coarse and declares the DerivativeControl
 * capability, and Tint takes those as WGSL's dpdxFine/dpdxCoarse -- so only
 * the cap was missing. A cap turned on without a test is a claim, so this is
 * the test.
 *
 * The varying ramps 0..1 across the target, so its derivative is 1/SIDE and
 * scaling by SIDE gives 1.0. Fine and coarse must agree on a linear function
 * -- that is what makes this checkable at all, since they are allowed to
 * differ only where the value varies within a quad.
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

static GLuint build(const char *body)
{
    static const char *vs_src =
        "#version 330 core\n"
        "out vec2 ramp;\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "  ramp = p * 0.5 + 0.5;\n"
        "}\n";
    char fs_src[640];
    snprintf(fs_src, sizeof(fs_src),
             "#version 330 core\n"
             "#extension GL_ARB_derivative_control : require\n"
             "in vec2 ramp;\n"
             "out vec4 colour;\n"
             "void main() { %s }\n", body);
    const char *fs_ptr = fs_src;
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs_src, NULL);
    glCompileShader(v);
    GLint ok = 0;
    glGetShaderiv(v, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs_ptr, NULL);
    glCompileShader(f);
    glGetShaderiv(f, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetShaderInfoLog(f, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    GLuint p = glCreateProgram();
    glAttachShader(p, v);
    glAttachShader(p, f);
    glLinkProgram(p);
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetProgramInfoLog(p, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return p;
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
    const int have = have_extension("GL_ARB_derivative_control");
    fprintf(stderr, "INFO version=%s derivative_control=%d\n",
            glGetString(GL_VERSION), have);
    if (!have) {
        fprintf(stderr, "INFO skipping: extension absent\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

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

    /* Each writes the scaled derivative into red, so 1.0 -> 255. Green
     * carries the y counterpart and blue the plain (unqualified) one, which
     * must agree on a linear ramp. */
    struct { const char *name; const char *body; } tests[] = {
        {"fine",
         "colour = vec4(dFdxFine(ramp.x) * 8.0, dFdyFine(ramp.y) * 8.0,"
         " dFdx(ramp.x) * 8.0, 1.0);"},
        {"coarse",
         "colour = vec4(dFdxCoarse(ramp.x) * 8.0, dFdyCoarse(ramp.y) * 8.0,"
         " dFdy(ramp.y) * 8.0, 1.0);"},
        {"fwidth-fine",
         "colour = vec4(fwidthFine(ramp.x) * 8.0, fwidthFine(ramp.y) * 8.0,"
         " fwidth(ramp.x) * 8.0, 1.0);"},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        GLuint program = build(tests[t].body);
        unsigned char px[4] = {0};
        GLenum error = GL_NO_ERROR;
        if (program) {
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(program);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
            error = glGetError();
            glDeleteProgram(program);
        }
        /* The magnitude, because a derivative's sign depends on which way the
         * implementation walks the quad and GL does not fix it. */
        fprintf(stderr, "INFO %s got=%u,%u,%u want=255,255,255 err=0x%04x\n",
                tests[t].name, px[0], px[1], px[2], (unsigned)error);
        result(tests[t].name,
               program && error == GL_NO_ERROR &&
               px[0] >= 250 && px[1] >= 250 && px[2] >= 250);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
