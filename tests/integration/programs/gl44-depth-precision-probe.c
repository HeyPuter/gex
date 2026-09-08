/* How many bits of depth an application actually gets.
 *
 * GL_DEPTH_COMPONENT24 and the unsized GL_DEPTH_COMPONENT are what almost
 * every application asks for, and st/mesa's candidate list for both is
 * Z24X8, X8Z24, *Z16_UNORM*, Z24_S8, S8_Z24 -- so a driver that supports
 * neither 24-bit spelling silently receives a 16-bit depth buffer. Nothing
 * fails: depth still tests, still writes, still reads back. It just has 1/256
 * of the resolution it asked for, which reaches the screen as z-fighting on
 * coplanar geometry and banding in anything that reconstructs position from
 * depth.
 *
 * Both halves are asked, because the reported size and the delivered
 * precision can disagree: the attachment query is what the application
 * branches on, and the round trip is what it actually gets.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>

#define SIDE 32

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 330 core\n"
    "const vec2 quad[4] = vec2[4](vec2(-1,-1), vec2(1,-1), vec2(-1,1), vec2(1,1));\n"
    "void main() { gl_Position = vec4(quad[gl_VertexID], 0.0, 1.0); }\n";
static const char *fs_src =
    "#version 330 core\n"
    "uniform float value;\n"
    "void main() { gl_FragDepth = value; }\n";

int main(void)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(dpy, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 1; }
    static const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                                       EGL_NONE};
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, NULL);
    if (ctx == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 1; }
    static const EGLint surf_attrs[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
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
    glUseProgram(prog);
    const GLint value_loc = glGetUniformLocation(prog, "value");

    glEnable(GL_DEPTH_TEST);
    glDepthFunc(GL_ALWAYS);
    glDepthMask(GL_TRUE);
    glViewport(0, 0, SIDE, SIDE);

    /* A value no 16-bit buffer can hold: it sits a third of the way between
     * two adjacent 16-bit codes, so a 16-bit round trip is off by ~5e-6 and a
     * 24-bit one by under 6e-8. */
    const float probe_value = 16384.5f / 65535.0f + 1.0e-6f;

    struct { const char *name; GLenum internal; int want_bits; } runs[] = {
        {"depth16", GL_DEPTH_COMPONENT16, 16},
        {"depth24", GL_DEPTH_COMPONENT24, 24},
        {"depth32f", GL_DEPTH_COMPONENT32F, 32},
        {"depth_default", GL_DEPTH_COMPONENT, 24},
    };

    for (unsigned i = 0; i < 4; ++i) {
        GLuint tex = 0, fbo = 0;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexImage2D(GL_TEXTURE_2D, 0, runs[i].internal, SIDE, SIDE, 0,
                     GL_DEPTH_COMPONENT, GL_FLOAT, NULL);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glGenFramebuffers(1, &fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT,
                               GL_TEXTURE_2D, tex, 0);
        glDrawBuffer(GL_NONE);
        glReadBuffer(GL_NONE);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
            fprintf(stderr, "INFO %s incomplete\n", runs[i].name);
            result(runs[i].name, 0);
            continue;
        }
        GLint bits = 0;
        glGetFramebufferAttachmentParameteriv(
            GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT,
            GL_FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE, &bits);

        glClearDepth(0.0);
        glClear(GL_DEPTH_BUFFER_BIT);
        glUniform1f(value_loc, probe_value);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        float got = -1.0f;
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_DEPTH_COMPONENT, GL_FLOAT,
                     &got);
        const float err = fabsf(got - probe_value);
        /* The tolerance the requested size implies, with a factor of two of
         * slack for the read-back conversion. */
        const float tolerance = 2.0f / (float)(1u << (runs[i].want_bits > 24
                                                      ? 24 : runs[i].want_bits));
        fprintf(stderr, "INFO %s bits=%d err=%.3e tolerance=%.3e\n",
                runs[i].name, bits, err, tolerance);
        result(runs[i].name, bits >= runs[i].want_bits && err <= tolerance);
        glDeleteFramebuffers(1, &fbo);
        glDeleteTextures(1, &tex);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
