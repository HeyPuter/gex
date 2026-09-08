/* What a fragment shader still does when the pass has no colour attachment.
 *
 * This is the shape of a shadow-map pass: depth attachment only, no colour.
 * An engine drawing an alpha-tested shadow caster -- foliage, a fence, a
 * chain-link texture -- binds a shader that samples its cutout mask and
 * discards, and often still declares an `out vec4` that nothing receives.
 * GL allows that: the colour write is simply dropped.
 *
 * A driver that answers "no colour targets" by leaving the fragment stage out
 * of the pipeline passes nothing here and looks like it works, because depth
 * is still written -- just by the rasterizer instead of by the shader. The
 * shadow comes out solid and the cutouts vanish.
 *
 *   discard_with_color_out  discards half the quad, and writes a colour
 *                           nothing collects -- the engine shape
 *   discard_no_color_out    the same with no colour output declared at all
 *   frag_depth_no_color     gl_FragDepth from a shader with no colour output
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>

#define SIDE 64
#define CLEARED 0.75f

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
    /* z = 0 so a rasterizer-written depth is 0.5 -- distinct from both the
     * clear and anything the shader writes. */
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

    /* Depth attachment only -- no colour, the whole point. */
    GLuint depth_tex = 0, fbo = 0;
    glGenTextures(1, &depth_tex);
    glBindTexture(GL_TEXTURE_2D, depth_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT24, SIDE, SIDE, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D,
                           depth_tex, 0);
    glDrawBuffer(GL_NONE);
    glReadBuffer(GL_NONE);
    fprintf(stderr, "INFO fbo status=0x%x (0x%x is complete)\n",
            glCheckFramebufferStatus(GL_FRAMEBUFFER), GL_FRAMEBUFFER_COMPLETE);

    /* uv.x < 0.5 is discarded, so the left half must keep the clear and the
     * right half must not. */
    GLuint prog_discard_color = make_prog(
        "#version 330 core\n"
        "in vec2 uv;\n"
        "out vec4 color;\n"
        "void main() {\n"
        "  if (uv.x < 0.5) discard;\n"
        "  color = vec4(1,0,0,1);\n"
        "  gl_FragDepth = 0.25;\n"
        "}\n");
    GLuint prog_discard_bare = make_prog(
        "#version 330 core\n"
        "in vec2 uv;\n"
        "void main() {\n"
        "  if (uv.x < 0.5) discard;\n"
        "  gl_FragDepth = 0.25;\n"
        "}\n");
    GLuint prog_depth_bare = make_prog(
        "#version 330 core\n"
        "in vec2 uv;\n"
        "void main() { gl_FragDepth = 0.25; }\n");

    glEnable(GL_DEPTH_TEST);
    glDepthFunc(GL_ALWAYS);
    glDepthMask(GL_TRUE);
    glDisable(GL_SCISSOR_TEST);
    glViewport(0, 0, SIDE, SIDE);

    struct { const char *name; GLuint prog; int discards; } runs[] = {
        {"discard_with_color_out", prog_discard_color, 1},
        {"discard_no_color_out", prog_discard_bare, 1},
        {"frag_depth_no_color", prog_depth_bare, 0},
    };
    for (unsigned i = 0; i < 3; ++i) {
        glClearDepth(CLEARED);
        glClear(GL_DEPTH_BUFFER_BIT);
        glUseProgram(runs[i].prog);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        float left = -1.0f, right = -1.0f;
        glReadPixels(SIDE / 4, SIDE / 2, 1, 1, GL_DEPTH_COMPONENT, GL_FLOAT,
                     &left);
        glReadPixels(3 * SIDE / 4, SIDE / 2, 1, 1, GL_DEPTH_COMPONENT, GL_FLOAT,
                     &right);
        fprintf(stderr, "INFO %s left=%.4f right=%.4f\n", runs[i].name, left,
                right);
        /* The discarded side keeps the clear; the drawn side takes 0.25. A
         * rasterizer-written depth is 0.5 on both sides, which fails each. */
        const int ok = near(right, 0.25f) &&
                       near(left, runs[i].discards ? CLEARED : 0.25f);
        result(runs[i].name, ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
