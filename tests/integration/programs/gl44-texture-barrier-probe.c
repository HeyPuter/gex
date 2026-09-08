/* glTextureBarrier: reading the texture you are rendering to, on purpose.
 *
 * A GL feedback loop -- sampling a texture that is also a colour attachment --
 * is undefined in general, and NV_texture_barrier is the API that makes one
 * defined: the barrier says every write so far is visible to every read after
 * it. Programmable blending is written this way.
 *
 * WebGPU forbids the binding outright, so the driver samples a *snapshot* of
 * the attachment instead. That is the right shape for the undefined case --
 * it is what keeps SuperTuxKart's seven draws a frame -- and it is only
 * correct across a barrier if the snapshot is retaken after it. A snapshot
 * cached from before the barrier reads stale texels, which is exactly what
 * the barrier promises will not happen.
 *
 * So the accumulation has to be *named*, not merely non-zero: each pass adds
 * a fixed amount to what it reads, so a stale read shows up as a value that
 * is too small by a whole pass rather than as garbage.
 *
 *   one_pass        a single write, so the barrier is not involved yet
 *   after_barrier   write, barrier, then read-and-add: 40 + 60 = 100
 *   two_barriers    and again: 40 + 60 + 60 = 160, which a snapshot taken
 *                   once reports as 100
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define SIDE 4

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
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
/* Writes a constant, for the first pass. */
static const char *fs_write =
    "#version 330 core\n"
    "layout(location = 0) out vec4 colour;\n"
    "void main() { colour = vec4(40.0/255.0, 0.0, 0.0, 1.0); }\n";
/* Reads the very texel it is about to write and adds to it. */
static const char *fs_accumulate =
    "#version 330 core\n"
    "uniform sampler2D self;\n"
    "layout(location = 0) out vec4 colour;\n"
    "void main() {\n"
    "  float had = texelFetch(self, ivec2(gl_FragCoord.xy), 0).r;\n"
    "  colour = vec4(had + 60.0/255.0, 0.0, 0.0, 1.0);\n"
    "}\n";

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

static GLuint link(const char *fs)
{
    GLuint v = compile(GL_VERTEX_SHADER, vs_src);
    GLuint f = compile(GL_FRAGMENT_SHADER, fs);
    if (!v || !f) return 0;
    GLuint p = glCreateProgram();
    glAttachShader(p, v);
    glAttachShader(p, f);
    glBindAttribLocation(p, 0, "pos");
    glLinkProgram(p);
    GLint ok = 0;
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[800] = {0};
        glGetProgramInfoLog(p, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return p;
}

static GLuint quad, fbo, tex;

static void pass(GLuint prog)
{
    glUseProgram(prog);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    const GLint at = glGetUniformLocation(prog, "self");
    if (at >= 0) glUniform1i(at, 0);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

static unsigned read_centre(void)
{
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    return px[0];
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

    /* Through eglGetProcAddress, not the link. Mesa's own libGL exports only
     * glTextureBarrierNV, so linking the core 4.5 name directly fails to
     * resolve at the first call -- and a loader (GLEW, glad, or the CTS's own
     * glw) is how every real application reaches it anyway. */
    void (*texture_barrier)(void) =
        (void (*)(void))eglGetProcAddress("glTextureBarrier");
    if (!texture_barrier)
        texture_barrier = (void (*)(void))eglGetProcAddress("glTextureBarrierNV");
    if (!texture_barrier) {
        fprintf(stderr, "INFO glTextureBarrier unavailable\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n");
        return 1;
    }
    static const float verts[] = {-1, -1, -1, 1, 1, -1, 1, 1};
    glGenBuffers(1, &quad);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_STATIC_DRAW);

    GLuint write_prog = link(fs_write);
    GLuint acc_prog = link(fs_accumulate);
    if (!write_prog || !acc_prog) { printf("FATAL programs\n"); return 1; }

    glViewport(0, 0, SIDE, SIDE);
    glDisable(GL_DEPTH_TEST);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);

    pass(write_prog);
    const unsigned one = read_centre();
    fprintf(stderr, "INFO one_pass=%u want 40\n", one);
    result("one_pass", one >= 38 && one <= 42);

    texture_barrier();
    pass(acc_prog);
    const unsigned two = read_centre();
    fprintf(stderr, "INFO after_barrier=%u want 100\n", two);
    result("after_barrier", two >= 97 && two <= 103);

    texture_barrier();
    pass(acc_prog);
    const unsigned three = read_centre();
    fprintf(stderr, "INFO two_barriers=%u want 160\n", three);
    result("two_barriers", three >= 157 && three <= 163);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
