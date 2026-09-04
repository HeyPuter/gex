/* glLogicOp: the last thing the 341-program sweep refused.
 *
 * A logic op combines the fragment with what the framebuffer already holds,
 * bitwise, on the integer representation -- XOR of two colours is not a
 * number float arithmetic can reach. WebGPU has no blend state for it at all,
 * so this driver reads a copy of the colour attachment in the fragment shader
 * and does the operation itself; the copy is the same snapshot the
 * feedback-loop path takes, because WebGPU will not let a pass sample the
 * texture it is rendering to.
 *
 * The idiom this exists for is the XOR rubber-band rectangle that 2D and CAD
 * software has drawn selections with for thirty years -- draw once to show
 * it, draw again to erase it. `line-xor` and `tri-logicop-xor` are the two
 * programs in the sweep that use it.
 *
 *   `xor_once`    XOR of a known source over a known destination, checked
 *                 against the value the arithmetic actually gives -- a driver
 *                 that ignored the op and wrote the source fails here
 *   `xor_twice`   XOR again restores the destination, which is the property
 *                 the rubber band relies on
 *   `invert`      INVERT ignores the source entirely, so a driver that
 *                 quietly wrote the source cannot pass it
 *   `and_or`      AND and OR against a destination carrying bits the source
 *                 does not, which tells them apart from each other and from
 *                 COPY
 *   `noop`        NOOP leaves the target exactly as it was
 *
 * Every case starts from a destination that is neither black nor white, so
 * "did nothing" and "wrote the source" are both visible.
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

static GLuint build(void) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 3.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 3.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "uniform vec4 source;\n"
        "out vec4 colour;\n"
        "void main() { colour = source; }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[600] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return prog;
}

/* Clear to `dst`, then draw `src` through `op`, and read back one pixel. */
static void run(GLuint program, GLenum op, const unsigned char dst[3],
                const unsigned char src[3], int draws, unsigned char out[4]) {
    glDisable(GL_COLOR_LOGIC_OP);
    glClearColor(dst[0] / 255.0f, dst[1] / 255.0f, dst[2] / 255.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(program);
    glUniform4f(glGetUniformLocation(program, "source"), src[0] / 255.0f,
                src[1] / 255.0f, src[2] / 255.0f, 1.0f);
    glEnable(GL_COLOR_LOGIC_OP);
    glLogicOp(op);
    for (int i = 0; i < draws; ++i)
        glDrawArrays(GL_TRIANGLES, 0, 3);
    glDisable(GL_COLOR_LOGIC_OP);
    memset(out, 0, 4);
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, out);
}

static int near(unsigned char got, int want) {
    const int d = (int)got - want;
    return d <= 1 && d >= -1;
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

    GLuint program = build();
    if (!program) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    /* Chosen so every operation gives a different answer: no channel where
     * src and dst share all their bits, and none where either is 0 or 255. */
    const unsigned char dst[3] = {0xF0, 0x3C, 0x55};
    const unsigned char src[3] = {0x0F, 0x5A, 0x33};
    unsigned char got[4];

    run(program, GL_XOR, dst, src, 1, got);
    GLenum error = glGetError();
    fprintf(stderr, "INFO xor_once got=%02x,%02x,%02x want=%02x,%02x,%02x "
            "err=0x%04x\n", got[0], got[1], got[2],
            dst[0] ^ src[0], dst[1] ^ src[1], dst[2] ^ src[2],
            (unsigned)error);
    result("xor_once", error == GL_NO_ERROR &&
                       near(got[0], dst[0] ^ src[0]) &&
                       near(got[1], dst[1] ^ src[1]) &&
                       near(got[2], dst[2] ^ src[2]));

    /* Twice restores the destination -- what a rubber band relies on. The
     * two draws are separate, so each sees what the other wrote. */
    run(program, GL_XOR, dst, src, 2, got);
    error = glGetError();
    fprintf(stderr, "INFO xor_twice got=%02x,%02x,%02x want=%02x,%02x,%02x\n",
            got[0], got[1], got[2], dst[0], dst[1], dst[2]);
    result("xor_twice", error == GL_NO_ERROR && near(got[0], dst[0]) &&
                        near(got[1], dst[1]) && near(got[2], dst[2]));

    run(program, GL_INVERT, dst, src, 1, got);
    error = glGetError();
    fprintf(stderr, "INFO invert got=%02x,%02x,%02x want=%02x,%02x,%02x\n",
            got[0], got[1], got[2], (unsigned char)~dst[0],
            (unsigned char)~dst[1], (unsigned char)~dst[2]);
    result("invert", error == GL_NO_ERROR &&
                     near(got[0], (unsigned char)~dst[0]) &&
                     near(got[1], (unsigned char)~dst[1]) &&
                     near(got[2], (unsigned char)~dst[2]));

    int and_or = 1;
    run(program, GL_AND, dst, src, 1, got);
    fprintf(stderr, "INFO and got=%02x,%02x,%02x want=%02x,%02x,%02x\n",
            got[0], got[1], got[2], dst[0] & src[0], dst[1] & src[1],
            dst[2] & src[2]);
    and_or = and_or && near(got[0], dst[0] & src[0]) &&
             near(got[1], dst[1] & src[1]) && near(got[2], dst[2] & src[2]);
    run(program, GL_OR, dst, src, 1, got);
    fprintf(stderr, "INFO or got=%02x,%02x,%02x want=%02x,%02x,%02x\n",
            got[0], got[1], got[2], dst[0] | src[0], dst[1] | src[1],
            dst[2] | src[2]);
    and_or = and_or && near(got[0], dst[0] | src[0]) &&
             near(got[1], dst[1] | src[1]) && near(got[2], dst[2] | src[2]);
    error = glGetError();
    result("and_or", and_or && error == GL_NO_ERROR);

    run(program, GL_NOOP, dst, src, 1, got);
    error = glGetError();
    fprintf(stderr, "INFO noop got=%02x,%02x,%02x want=%02x,%02x,%02x\n",
            got[0], got[1], got[2], dst[0], dst[1], dst[2]);
    result("noop", error == GL_NO_ERROR && near(got[0], dst[0]) &&
                   near(got[1], dst[1]) && near(got[2], dst[2]));

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
