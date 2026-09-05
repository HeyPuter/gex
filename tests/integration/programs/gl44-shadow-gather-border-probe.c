/* textureGather on a depth-comparison sampler clamped to a border.
 *
 * WGSL has textureGatherCompare, so an ordinary shadow gather goes through
 * natively. What has no native form is a *border*: each of the four texels in
 * the footprint can fall outside the texture independently, and WebGPU has no
 * border colour to begin with. The driver decomposes such a gather into four
 * ordinary shadow samples at the four texel centres and gives each of them its
 * border separately.
 *
 * The four points below straddle the boundary in different ways -- one wholly
 * inside, two on a corner where three of the four texels are border, and one
 * wholly outside -- so a decomposition that got the rotation, the footprint or
 * the comparison wrong shows up as a different bit pattern rather than as a
 * uniform result.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>

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

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (d == EGL_NO_DISPLAY || !eglInitialize(d, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE };
    EGLConfig cfg = NULL; EGLint n = 0;
    if (!eglChooseConfig(d, ca, &cfg, 1, &n) || n < 1) return 2;
    /* textureGather on a shadow sampler is GLSL 4.00. */
    const EGLint cx[] = { EGL_CONTEXT_MAJOR_VERSION, 4,
                          EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE };
    const EGLint sa[] = { EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE };
    EGLContext c = eglCreateContext(d, cfg, EGL_NO_CONTEXT, cx);
    EGLSurface s = eglCreatePbufferSurface(d, cfg, sa);
    if (c == EGL_NO_CONTEXT || s == EGL_NO_SURFACE ||
        !eglMakeCurrent(d, s, s, c)) return 2;

    /* Every texel a different depth, so which one landed where is visible. */
    GLuint t = 0;
    glActiveTexture(GL_TEXTURE0);
    glGenTextures(1, &t);
    glBindTexture(GL_TEXTURE_2D, t);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_DEPTH_COMPONENT32F, 4, 4);
    float depth[16];
    for (int i = 0; i < 16; ++i) depth[i] = (float)i / 16.0f;
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 4, 4, GL_DEPTH_COMPONENT,
                    GL_FLOAT, depth);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_BORDER);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_BORDER);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_MODE,
                    GL_COMPARE_REF_TO_TEXTURE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_FUNC, GL_LEQUAL);
    const float border[4] = {1.0f, 1.0f, 1.0f, 1.0f};
    glTexParameterfv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, border);

    const char *vs =
        "#version 400 core\n"
        "flat out int v_id;\n"
        "void main(){\n"
        "  gl_Position = vec4((2.0*(float(gl_VertexID)+0.5)/4.0)-1.0,\n"
        "                     0.0, 0.0, 1.0);\n"
        "  v_id = gl_VertexID;\n}\n";
    const char *fs =
        "#version 400 core\n"
        "uniform sampler2DShadow shadow;\n"
        "flat in int v_id;\n"
        "out uint outc;\n"
        "void main(){\n"
        "  vec2 p = vec2(0.5);\n"
        "  if (v_id == 1) p = vec2(0.0);\n"
        "  if (v_id == 2) p = vec2(1.0);\n"
        "  if (v_id == 3) p = vec2(-0.5);\n"
        "  vec4 g = textureGather(shadow, p, 0.55);\n"
        "  outc = uint(g.x > 0.5) | (uint(g.y > 0.5) << 1)\n"
        "       | (uint(g.z > 0.5) << 2) | (uint(g.w > 0.5) << 3);\n}\n";

    GLuint p = glCreateProgram();
    glAttachShader(p, sh(GL_VERTEX_SHADER, vs, "vs"));
    glAttachShader(p, sh(GL_FRAGMENT_SHADER, fs, "fs"));
    glLinkProgram(p);
    GLint linked = 0; glGetProgramiv(p, GL_LINK_STATUS, &linked);
    if (!linked) { char log[2000] = {0};
                   glGetProgramInfoLog(p, 1999, NULL, log);
                   fprintf(stderr, "INFO link: %s\n", log); }
    glUseProgram(p);
    glUniform1i(glGetUniformLocation(p, "shadow"), 0);

    GLuint rt = 0, fbo = 0;
    glGenTextures(1, &rt);
    glActiveTexture(GL_TEXTURE1);
    glBindTexture(GL_TEXTURE_2D, rt);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, 4, 1);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, rt, 0);
    glViewport(0, 0, 4, 1);
    const GLuint clear[4] = {0xffffu, 0xffffu, 0xffffu, 0xffffu};
    glClearBufferuiv(GL_COLOR, 0, clear);

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glDrawArrays(GL_POINTS, 0, 4);

    GLuint got[4] = {0, 0, 0, 0};
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    glReadPixels(0, 0, 4, 1, GL_RED_INTEGER, GL_UNSIGNED_INT, got);
    const GLenum error = glGetError();
    /* The bit patterns themselves are the comparison, rather than a pass count:
     * the reference driver decides what they should be, and which one differs
     * says which texel of the footprint went astray. */
    const int ok = linked && error == GL_NO_ERROR;
    printf("gl44 shadow gather border %s (centre=%u corner00=%u corner11=%u "
           "outside=%u)\n", ok ? "ok" : "FAILED",
           got[0], got[1], got[2], got[3]);
    (void)cases; (void)passed; (void)result;
    return ok ? 0 : 1;
}
