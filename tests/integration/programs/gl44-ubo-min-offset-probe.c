/* A uniform buffer bound at the smallest offset GL says is legal.
 *
 * GL_UNIFORM_BUFFER_OFFSET_ALIGNMENT is what every glBindBufferRange offset
 * must be a multiple of, so the smallest legal non-zero offset is the
 * alignment itself. This binds exactly that and checks the draw sees the data.
 *
 * It exists because that invariant broke the moment the driver stopped
 * reporting 256. The driver asks the adapter for the alignment now and reports
 * what it was granted -- 32 natively -- while a check two thousand lines away
 * still refused any offset that was not a multiple of **256**, and refusing a
 * constant buffer calls gex_fail, which latches the context: every later draw
 * becomes a no-op while the application keeps presenting at full rate. So the
 * failure this looks for is not an error code, it is a frame with nothing in
 * it.
 *
 * A cap that gets looser has to be followed into every check that assumed the
 * old value, and no existing gate caught this one: st/mesa aligns its own
 * uniform uploads, so only an application choosing its own tighter offset --
 * which is ordinary -- would have hit it.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases, passed;
static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-30s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 32, EGL_HEIGHT, 32, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;

    GLint align = 0;
    glGetIntegerv(GL_UNIFORM_BUFFER_OFFSET_ALIGNMENT, &align);
    drain();
    fprintf(stderr, "INFO version=%s uniform_offset_alignment=%d\n",
            (const char *)glGetString(GL_VERSION), align);
    if (align <= 0) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 0; }

    static const char *vs =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 3.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 3.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0); }\n";
    static const char *fs =
        "#version 330 core\n"
        "layout(std140) uniform Block { vec4 tint; };\n"
        "out vec4 colour;\n"
        "void main() { colour = tint; }\n";
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs, NULL); glCompileShader(v);
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs, NULL); glCompileShader(f);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, v); glAttachShader(prog, f);
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    eq("program_links", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n",
                          cases, passed, cases - passed); return 1; }
    glUniformBlockBinding(prog, glGetUniformBlockIndex(prog, "Block"), 0);

    /* The data sits at exactly one alignment unit in, so the offset is the
       smallest non-zero one GL permits on this implementation. */
    const GLsizeiptr offset = align;
    unsigned char bytes[4096];
    memset(bytes, 0, sizeof(bytes));
    const float tint[4] = { 0.0f, 1.0f, 0.0f, 1.0f };   /* green */
    memcpy(bytes + offset, tint, sizeof(tint));
    GLuint ubo = 0;
    glGenBuffers(1, &ubo);
    glBindBuffer(GL_UNIFORM_BUFFER, ubo);
    glBufferData(GL_UNIFORM_BUFFER, sizeof(bytes), bytes, GL_STATIC_DRAW);
    glBindBufferRange(GL_UNIFORM_BUFFER, 0, ubo, offset, sizeof(tint) * 4);
    eq("bind_at_min_offset_is_clean", glGetError(), GL_NO_ERROR);

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glUseProgram(prog);
    glClearColor(1.0f, 0.0f, 0.0f, 1.0f);           /* red, so a lost draw shows */
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glFinish();
    drain();

    unsigned char px[4] = {0, 0, 0, 0};
    glReadPixels(16, 16, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    drain();
    fprintf(stderr, "INFO centre pixel %u,%u,%u,%u (red means the draw was "
            "lost)\n", px[0], px[1], px[2], px[3]);
    /* Green is the uniform's value arriving; red is the clear showing through,
       which is what a refused constant buffer and a latched context look
       like from the application's side. */
    eq("draw_used_the_uniform", px[1] > 200 && px[0] < 60, 1);

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
