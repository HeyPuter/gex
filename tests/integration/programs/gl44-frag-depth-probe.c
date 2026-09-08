/* gl_FragDepth, and sampling a depth texture to feed it.
 *
 * These are the two halves of what a drawn depth blit does, split apart
 * because they fail identically from the outside: the destination ends up
 * holding the quad's own interpolated z instead of the value the shader
 * meant to write, and 0.5 -- NDC zero remapped into [0,1] -- looks like a
 * depth rather than like a bug.
 *
 *   constant      gl_FragDepth = 0.25, nothing sampled
 *   from_sampler  gl_FragDepth = texture(depth_tex, uv).r, source is 0.25
 *   sample_only   the same sample written to *colour*, so a wrong sample and
 *                 an ignored gl_FragDepth can be told apart
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>

#define SIDE 64
#define SRC_DEPTH 0.25f
#define DST_DEPTH 0.75f

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
    "out vec2 uv;\n"
    "void main() {\n"
    "  vec2 p = quad[gl_VertexID];\n"
    "  uv = p * 0.5 + 0.5;\n"
    /* z = 0 so the interpolated window depth is exactly 0.5 -- the value a
     * lost gl_FragDepth leaves behind. */
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "}\n";

static GLuint make_prog(const char *fs_src)
{
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLint ok = 0;
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(fs, sizeof log, NULL, log);
        fprintf(stderr, "INFO fs compile: %s\n", log);
    }
    GLuint p = glCreateProgram();
    glAttachShader(p, vs);
    glAttachShader(p, fs);
    glLinkProgram(p);
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetProgramInfoLog(p, sizeof log, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
    }
    return p;
}

static int near(float got, float want) { return fabsf(got - want) < 0.02f; }

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

    /* The source depth texture, sampled by the shaders below. */
    GLuint src_tex = 0, src_fbo = 0;
    glGenTextures(1, &src_tex);
    glBindTexture(GL_TEXTURE_2D, src_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT24, SIDE, SIDE, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &src_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, src_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D,
                           src_tex, 0);
    glViewport(0, 0, SIDE, SIDE);
    glClearDepth(SRC_DEPTH);
    glClear(GL_DEPTH_BUFFER_BIT);

    /* The destination: depth to write into, plus colour for sample_only. */
    GLuint dst_depth = 0, dst_color = 0, dst_fbo = 0;
    glGenTextures(1, &dst_depth);
    glBindTexture(GL_TEXTURE_2D, dst_depth);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT24, SIDE, SIDE, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenTextures(1, &dst_color);
    glBindTexture(GL_TEXTURE_2D, dst_color);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &dst_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, dst_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           dst_color, 0);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D,
                           dst_depth, 0);

    GLuint prog_const = make_prog(
        "#version 330 core\n"
        "in vec2 uv;\n"
        "out vec4 color;\n"
        "void main() { color = vec4(1,0,0,1); gl_FragDepth = 0.25; }\n");
    GLuint prog_sampled = make_prog(
        "#version 330 core\n"
        "uniform sampler2D depth_tex;\n"
        "in vec2 uv;\n"
        "out vec4 color;\n"
        "void main() {\n"
        "  color = vec4(0,1,0,1);\n"
        "  gl_FragDepth = texture(depth_tex, uv).r;\n"
        "}\n");
    GLuint prog_color = make_prog(
        "#version 330 core\n"
        "uniform sampler2D depth_tex;\n"
        "in vec2 uv;\n"
        "out vec4 color;\n"
        "void main() { color = vec4(texture(depth_tex, uv).r, 0, 0, 1); }\n");

    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, src_tex);

    /* Depth write always wins, so whatever the shader emits lands. */
    glEnable(GL_DEPTH_TEST);
    glDepthFunc(GL_ALWAYS);
    glDepthMask(GL_TRUE);
    glDisable(GL_SCISSOR_TEST);

    struct { const char *name; GLuint prog; float want; } depth_runs[] = {
        {"constant", prog_const, 0.25f},
        {"from_sampler", prog_sampled, 0.25f},
    };
    for (unsigned i = 0; i < 2; ++i) {
        glBindFramebuffer(GL_FRAMEBUFFER, dst_fbo);
        glViewport(0, 0, SIDE, SIDE);
        glClearDepth(DST_DEPTH);
        glClear(GL_DEPTH_BUFFER_BIT | GL_COLOR_BUFFER_BIT);
        glUseProgram(depth_runs[i].prog);
        glUniform1i(glGetUniformLocation(depth_runs[i].prog, "depth_tex"), 0);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        float d = -1.0f;
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_DEPTH_COMPONENT, GL_FLOAT, &d);
        fprintf(stderr, "INFO %s depth=%.4f\n", depth_runs[i].name, d);
        result(depth_runs[i].name, near(d, depth_runs[i].want));
    }

    /* And the sample on its own, into colour. */
    glBindFramebuffer(GL_FRAMEBUFFER, dst_fbo);
    glDisable(GL_DEPTH_TEST);
    glClearColor(0, 0, 1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog_color);
    glUniform1i(glGetUniformLocation(prog_color, "depth_tex"), 0);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    fprintf(stderr, "INFO sample_only r=%u (0.25 -> ~64)\n", px[0]);
    result("sample_only", px[0] > 55 && px[0] < 75);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
