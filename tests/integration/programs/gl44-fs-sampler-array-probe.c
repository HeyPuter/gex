/* A fragment shader reading an integer sampler array by a dynamic index.
 *
 * `usampler2D s[16]` indexed by a value the compiler cannot fold is ordinary
 * GLSL, and WGSL has no sampler arrays at all, so the driver unrolls the index
 * into one constant-index copy per unit. It also cannot *sample* an integer
 * texture, so each of those copies becomes a fetch.
 *
 * This is a regression test for a hard cut at eight units:
 *
 *     llvmpipe:  0 1 2 3 4 5 6 7  8  9 10 11 12 13 14 15
 *     gex_wgpu:  0 1 2 3 4 5 6 7  0  0  0  0  0  0  0  0
 *
 * The sample-type classification in gex_wgpu_collect_shader_state kept its
 * shadow/float/uint/sint slot masks in `uint8_t`, so `1u << slot` for slots
 * 8..15 was truncated to nothing. Those units kept the default *Float* sample
 * type; gex_wgpu_unusable_texture_slots then compared Float against the bound
 * integer view, declared the slot unsatisfiable, and gex_wgpu_omit_textures
 * rewrote its fetches to a zero constant -- correct machinery reaching the
 * wrong conclusion from a truncated mask. The WGSL said so plainly: eight
 * textureLoad calls and a trailing
 * `select(x, 0u, index >= 8)`.
 *
 * The cut was fragment-only because the omit rewrite is: the vertex and
 * geometry stages unroll the same array correctly and never consult that
 * mask. It also cost KHR-GL45.geometry_shader.limits.max_combined_texture_units,
 * which sums units 0..point-1 across three stages, so its first wrong point
 * was 9 -- the first sum to include unit 8, short by exactly that unit.
 *
 * The masks are `uint16_t` now, matching GEX_WGPU_MAX_SAMPLERS. Depth and
 * shadow samplers above unit 7 were mis-typed by the same truncation.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <stdlib.h>

#define N 16
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
    /* 4.0 at least: GLSL 3.30 forbids indexing a sampler array with a
     * non-constant expression, so the shader below would not compile there --
     * on either driver, which makes the probe agree by failing to run. */
    const EGLint cx[] = { EGL_CONTEXT_MAJOR_VERSION, 4,
                          EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE };
    const EGLint sa[] = { EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE };
    EGLContext c = eglCreateContext(d, cfg, EGL_NO_CONTEXT, cx);
    EGLSurface s = eglCreatePbufferSurface(d, cfg, sa);
    if (c == EGL_NO_CONTEXT || s == EGL_NO_SURFACE ||
        !eglMakeCurrent(d, s, s, c)) return 2;

    /* Unit i holds the value i, so a zero names the unit that produced it. */
    GLuint tex[N];
    glGenTextures(N, tex);
    for (int i = 0; i < N; ++i) {
        glActiveTexture(GL_TEXTURE0 + i);
        glBindTexture(GL_TEXTURE_2D, tex[i]);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, 1, 1);
        GLuint v = (GLuint)i;
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 1, 1, GL_RED_INTEGER,
                        GL_UNSIGNED_INT, &v);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    }

    char vs[512], fs[1024];
    snprintf(vs, sizeof vs,
        "#version 400 core\n"
        "flat out int v_id;\n"
        "void main(){\n"
        "  gl_Position = vec4((2.0*(float(gl_VertexID)+0.5)/%d.0)-1.0,\n"
        "                     0.0, 0.0, 1.0);\n"
        "  v_id = gl_VertexID;\n}\n", N);
    snprintf(fs, sizeof fs,
        "#version 400 core\n"
        "uniform highp usampler2D sampler[%d];\n"
        "flat in int v_id;\n"
        "out uint outc;\n"
        "void main(){ outc = texture(sampler[v_id], vec2(0.0)).r; }\n", N);

    GLuint p = glCreateProgram();
    glAttachShader(p, sh(GL_VERTEX_SHADER, vs, "vs"));
    glAttachShader(p, sh(GL_FRAGMENT_SHADER, fs, "fs"));
    glLinkProgram(p);
    GLint linked = 0; glGetProgramiv(p, GL_LINK_STATUS, &linked);
    if (!linked) { char log[2000] = {0};
                   glGetProgramInfoLog(p, 1999, NULL, log);
                   fprintf(stderr, "INFO link: %s\n", log); }
    glUseProgram(p);
    for (int i = 0; i < N; ++i) {
        char nm[32]; snprintf(nm, sizeof nm, "sampler[%d]", i);
        glUniform1i(glGetUniformLocation(p, nm), i);
    }

    GLuint rt = 0, fbo = 0;
    glGenTextures(1, &rt);
    glActiveTexture(GL_TEXTURE0 + N);
    glBindTexture(GL_TEXTURE_2D, rt);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, N, 1);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, rt, 0);
    glViewport(0, 0, N, 1);
    const GLuint clear[4] = {0, 0, 0, 0};
    glClearBufferuiv(GL_COLOR, 0, clear);

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glDrawArrays(GL_POINTS, 0, N);

    GLuint got[N]; for (int i = 0; i < N; ++i) got[i] = 0xffffffffu;
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    glReadPixels(0, 0, N, 1, GL_RED_INTEGER, GL_UNSIGNED_INT, got);
    const GLenum error = glGetError();

    fprintf(stderr, "INFO units:");
    for (int i = 0; i < N; ++i) fprintf(stderr, " %u", got[i]);
    fprintf(stderr, " err=0x%04x\n", (unsigned)error);

    int low = 1, high = 1;
    for (int i = 0; i < 8; ++i) if (got[i] != (GLuint)i) low = 0;
    for (int i = 8; i < N; ++i) if (got[i] != (GLuint)i) high = 0;
    result("units_0_7", linked && error == GL_NO_ERROR && low);
    result("units_8_15", linked && error == GL_NO_ERROR && high);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
