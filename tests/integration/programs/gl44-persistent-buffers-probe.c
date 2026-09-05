/* Many persistently mapped coherent buffers at once.
 *
 * ARB_buffer_storage lets an application keep a buffer mapped while it draws
 * from it and write through the pointer with no further API call. WebGPU has
 * nothing of the kind, so the driver shadows such a buffer and uploads it
 * before each submit -- and it tracked the mappings in a *fixed array of
 * eight*. The ninth onwards were silently not kept coherent: their writes
 * never reached the device, and every draw from them read whatever the buffer
 * held when it was created.
 *
 * That is Minecraft's title screen. Its GUI draws from a pool of persistent
 * buffers, so the elements whose buffer landed past the eighth slot -- the
 * panorama and every piece of text -- drew from stale memory while the logo
 * and the buttons were there. The picture looked like a font or a texture
 * problem and was neither.
 *
 * Forty buffers, written twice. Forty because sixteen is not enough to catch
 * it: the tracking array grows in powers of two from sixteen, so a probe with
 * sixteen mappings never reaches the limit and passes with the defect present.
 * The count has to exceed whatever the driver would settle on, not merely the
 * eight the array was originally declared with. Both rounds are checked; the
 * second is the one that matters, since the first can be served by the upload
 * that happens when a buffer is created.
 *
 * With the defect the pattern is unmistakable: slots one to seven carry their
 * values, everything from the eighth on reads zero, and slot zero reads zero
 * too because every untracked point drew at x = 0 on top of it.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

#define N 40
static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static GLuint sh(GLenum stage, const char *src, const char *what) {
    GLuint s = glCreateShader(stage);
    glShaderSource(s, 1, &src, NULL); glCompileShader(s);
    GLint ok = 0; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2000] = {0}; glGetShaderInfoLog(s, 1999, NULL, log);
               fprintf(stderr, "INFO %s compile: %s\n", what, log); }
    return s;
}

static GLuint buf[N];
static GLuint *map[N];

/* One point per buffer, each reading its position and value from its own
 * persistent mapping. */
static void draw_round(GLuint vao, unsigned bias) {
    for (int i = 0; i < N; ++i) {
        map[i][0] = (GLuint)i;
        map[i][1] = (GLuint)i + bias;
    }
    for (int i = 0; i < N; ++i) {
        glBindBuffer(GL_ARRAY_BUFFER, buf[i]);
        glVertexAttribIPointer(0, 2, GL_UNSIGNED_INT, 0, NULL);
        glEnableVertexAttribArray(0);
        glDrawArrays(GL_POINTS, 0, 1);
    }
    (void)vao;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (d == EGL_NO_DISPLAY || !eglInitialize(d, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE };
    EGLConfig cfg = NULL; EGLint n = 0;
    if (!eglChooseConfig(d, ca, &cfg, 1, &n) || n < 1) return 2;
    /* 4.4 for ARB_buffer_storage. */
    const EGLint cx[] = { EGL_CONTEXT_MAJOR_VERSION, 4,
                          EGL_CONTEXT_MINOR_VERSION, 4, EGL_NONE };
    const EGLint sa[] = { EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE };
    EGLContext c = eglCreateContext(d, cfg, EGL_NO_CONTEXT, cx);
    EGLSurface s = eglCreatePbufferSurface(d, cfg, sa);
    if (c == EGL_NO_CONTEXT || s == EGL_NO_SURFACE ||
        !eglMakeCurrent(d, s, s, c)) return 2;

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    const GLbitfield flags = GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT |
                             GL_MAP_COHERENT_BIT;
    glGenBuffers(N, buf);
    for (int i = 0; i < N; ++i) {
        glBindBuffer(GL_ARRAY_BUFFER, buf[i]);
        glBufferStorage(GL_ARRAY_BUFFER, 2 * sizeof(GLuint), NULL, flags);
        map[i] = glMapBufferRange(GL_ARRAY_BUFFER, 0, 2 * sizeof(GLuint),
                                  flags);
        if (!map[i]) { fprintf(stderr, "INFO map %d failed\n", i); return 2; }
    }

    const char *vs =
        "#version 440 core\n"
        "layout(location = 0) in uvec2 a;\n"
        "flat out uint v_val;\n"
        "void main(){\n"
        "  gl_Position = vec4((2.0*(float(a.x)+0.5)/40.0)-1.0,\n"
        "                     0.0, 0.0, 1.0);\n"
        "  v_val = a.y;\n}\n";
    const char *fs =
        "#version 440 core\n"
        "flat in uint v_val;\n"
        "out uint outc;\n"
        "void main(){ outc = v_val; }\n";

    GLuint p = glCreateProgram();
    glAttachShader(p, sh(GL_VERTEX_SHADER, vs, "vs"));
    glAttachShader(p, sh(GL_FRAGMENT_SHADER, fs, "fs"));
    glLinkProgram(p);
    GLint linked = 0; glGetProgramiv(p, GL_LINK_STATUS, &linked);
    if (!linked) { char log[2000] = {0};
                   glGetProgramInfoLog(p, 1999, NULL, log);
                   fprintf(stderr, "INFO link: %s\n", log); }
    glUseProgram(p);

    GLuint rt = 0, fbo = 0;
    glGenTextures(1, &rt);
    glBindTexture(GL_TEXTURE_2D, rt);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, N, 1);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, rt, 0);
    glViewport(0, 0, N, 1);

    GLuint got[N];
    int ok[2] = {0, 0};
    /* Round 0 writes i+1, round 1 writes i+101. Neither value is zero, so a
     * pixel nothing drew to is distinguishable from one drawn with a stale
     * value, and the two rounds cannot be confused with each other. */
    for (int round = 0; round < 2; ++round) {
        const unsigned bias = round == 0 ? 1u : 101u;
        const GLuint clear[4] = {0, 0, 0, 0};
        glClearBufferuiv(GL_COLOR, 0, clear);
        draw_round(vao, bias);
        glReadBuffer(GL_COLOR_ATTACHMENT0);
        glReadPixels(0, 0, N, 1, GL_RED_INTEGER, GL_UNSIGNED_INT, got);
        fprintf(stderr, "INFO round%d:", round);
        for (int i = 0; i < N; ++i) fprintf(stderr, " %u", got[i]);
        fprintf(stderr, "\n");
        ok[round] = 1;
        for (int i = 0; i < N; ++i)
            if (got[i] != (GLuint)i + bias) ok[round] = 0;
    }
    const GLenum error = glGetError();
    result("first_write", linked && error == GL_NO_ERROR && ok[0]);
    result("rewrite_through_mapping", linked && error == GL_NO_ERROR && ok[1]);
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
