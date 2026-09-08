/* glDepthRange, including the inverted form reverse-Z is spelled with.
 *
 * GL's window depth is (f-n)/2 * z_ndc + (n+f)/2, so glDepthRange(n, f) with
 * n > f gives a negative slope. WebGPU's viewport cannot take that -- it
 * requires minDepth <= maxDepth -- and the driver used to decline the draw,
 * which is a poor trade: an inverted range is how reverse-Z is written and
 * modern renderers use it as a matter of course.
 *
 * The emulation sorts the range for the viewport and folds the difference
 * into clip z in the vertex shader. Getting the *sign* right is easy and
 * getting the offset right is not, so this pins three ranges rather than one:
 * a partial inverted range fails a sign-only fix while an inverted [1,0]
 * passes it.
 *
 *   normal_range        glDepthRange(0, 1),     z_ndc 0.5 -> 0.75
 *   inverted_full       glDepthRange(1, 0),     z_ndc 0.5 -> 0.25
 *   inverted_partial    glDepthRange(0.8, 0.2), z_ndc 0.5 -> 0.35
 *   normal_partial      glDepthRange(0.2, 0.8), z_ndc 0.5 -> 0.65
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <math.h>

#define SIDE 8

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
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.5, 1.0); }\n";
static const char *fs_src =
    "#version 330 core\n"
    "layout(location = 0) out vec4 colour;\n"
    "void main() { colour = vec4(1.0, 0.0, 0.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *src)
{
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL);
    glCompileShader(sh);
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[800] = {0};
        glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    return sh;
}

static GLuint prog, quad, fbo;

static float draw_and_read(float near_v, float far_v)
{
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glViewport(0, 0, SIDE, SIDE);
    glDepthRange(near_v, far_v);
    glEnable(GL_DEPTH_TEST);
    glDepthFunc(GL_ALWAYS);
    glClearDepth(0.0);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    glUseProgram(prog);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    float depth = -1.0f;
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_DEPTH_COMPONENT, GL_FLOAT,
                 &depth);
    if (glGetError() != GL_NO_ERROR)
        return -1.0f;
    return depth;
}

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

    GLuint colour_tex = 0, depth_tex = 0;
    glGenTextures(1, &colour_tex);
    glBindTexture(GL_TEXTURE_2D, colour_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenTextures(1, &depth_tex);
    glBindTexture(GL_TEXTURE_2D, depth_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT32F, SIDE, SIDE, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, colour_tex, 0);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D,
                           depth_tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n");
        return 1;
    }

    static const float verts[] = {-1, -1, -1, 1, 1, -1, 1, 1};
    glGenBuffers(1, &quad);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_STATIC_DRAW);

    GLuint v = compile(GL_VERTEX_SHADER, vs_src);
    GLuint f = compile(GL_FRAGMENT_SHADER, fs_src);
    if (!v || !f) { printf("FATAL compile\n"); return 1; }
    prog = glCreateProgram();
    glAttachShader(prog, v);
    glAttachShader(prog, f);
    glBindAttribLocation(prog, 0, "pos");
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) { printf("FATAL link\n"); return 1; }

    struct { const char *name; float n, f, want; } trials[] = {
        {"normal_range",     0.0f, 1.0f, 0.75f},
        {"inverted_full",    1.0f, 0.0f, 0.25f},
        {"inverted_partial", 0.8f, 0.2f, 0.35f},
        {"normal_partial",   0.2f, 0.8f, 0.65f},
    };
    for (unsigned i = 0; i < sizeof(trials) / sizeof(trials[0]); ++i) {
        const float got = draw_and_read(trials[i].n, trials[i].f);
        fprintf(stderr, "INFO %-17s range=%.1f..%.1f got=%.4f want=%.4f\n",
                trials[i].name, trials[i].n, trials[i].f, got, trials[i].want);
        result(trials[i].name, got >= 0.0f &&
               fabsf(got - trials[i].want) < 0.01f);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
