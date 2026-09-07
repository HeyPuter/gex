/* A samplerBuffer whose texture unit has nothing bound.
 *
 * A shader may declare a buffer texture and never have one bound: a uniform
 * left at its default, a branch not taken, a debug path compiled in. GL reads
 * **zero** from an unbound or incomplete texture and raises no error, and a
 * texelFetch out of a buffer texture is the same rule.
 *
 * This driver called gex_fail() for it in the vertex and fragment stages, and
 * gex_fail **latches the context**: one shader declaring an unbound
 * samplerBuffer dropped every later draw in the program, not just its own.
 * Nothing reports a latched context -- glGetError stays clean -- so an
 * application saw the rest of its frame disappear with no explanation. The
 * driver now binds the same zero-filled placeholder it already used for an
 * unbound *storage block*.
 *
 *   `unbound_reads_zero`   texelFetch from an unbound usamplerBuffer is 0,
 *                          the draw happens, and no GL error is raised
 *   `bound_survives`       a second unit that IS bound still reads its own
 *                          texel, so the placeholder is not bound over it
 *   `later_draw_runs`      a plain draw afterwards still reaches the target.
 *                          This is the case latching broke, and a probe that
 *                          checks only the first draw cannot see it
 *
 * `later_draw_runs` is last and trivial on purpose: if the context latched,
 * it fails while the two before it may still pass.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

#define SIDE 16

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint build(const char *fs_body) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    char fs_src[900];
    snprintf(fs_src, sizeof(fs_src), "%s", fs_body);
    const char *fs_ptr = fs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_ptr, NULL);
    glCompileShader(fs);
    GLint ok = 0;
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
    const EGLint surface_attributes[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE,
                                         EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
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

    /* Unit 1 gets a real buffer texture whose texel 0 is 200; unit 0 gets
     * nothing. Reading the two into different channels means a driver that
     * binds the placeholder over the wrong unit shows as a swapped pair
     * rather than as a plausible colour. */
    const unsigned char bytes[4] = {200, 0, 0, 0};
    GLuint buffer = 0, tex = 0;
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_TEXTURE_BUFFER, buffer);
    glBufferData(GL_TEXTURE_BUFFER, sizeof(bytes), bytes, GL_STATIC_DRAW);
    glGenTextures(1, &tex);
    glActiveTexture(GL_TEXTURE1);
    glBindTexture(GL_TEXTURE_BUFFER, tex);
    glTexBuffer(GL_TEXTURE_BUFFER, GL_R8UI, buffer);
    glActiveTexture(GL_TEXTURE0);

    GLuint prog = build(
        "#version 330 core\n"
        "uniform usamplerBuffer absent;\n"
        "uniform usamplerBuffer present;\n"
        "out vec4 colour;\n"
        "void main() {\n"
        "  uint a = texelFetch(absent, 0).r;\n"
        "  uint b = texelFetch(present, 0).r;\n"
        "  colour = vec4(float(a) / 255.0, float(b) / 255.0, 0.0, 1.0);\n"
        "}\n");
    if (!prog) {
        /* A link failure here is the defect: GL permits this shader. */
        result("unbound_reads_zero", 0);
        result("bound_survives", 0);
        result("later_draw_runs", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 0;
    }
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "absent"), 0);
    glUniform1i(glGetUniformLocation(prog, "present"), 1);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char px[4] = {77, 77, 77, 77};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    const GLenum err = glGetError();
    fprintf(stderr, "INFO absent=%u present=%u err=0x%04x\n",
            px[0], px[1], (unsigned)err);
    result("unbound_reads_zero", px[0] == 0 && err == GL_NO_ERROR);
    result("bound_survives", px[1] == 200);

    /* The latch test. A draw that touches no buffer texture at all: it can
     * only fail if the draw above failed the whole context. */
    GLuint plain = build(
        "#version 330 core\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(0.0, 0.0, 1.0, 1.0); }\n");
    int later_ok = 0;
    if (plain) {
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(plain);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        unsigned char after[4] = {0, 0, 0, 0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     after);
        fprintf(stderr, "INFO later blue=%u\n", after[2]);
        later_ok = after[2] > 200;
    }
    result("later_draw_runs", later_ok);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
