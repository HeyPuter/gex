/* Depth-comparison samplers past the eighth slot.
 *
 * The companion to gl44-fs-sampler-array-probe. The sample-type classification
 * in gex_wgpu_collect_shader_state kept its shadow/float/uint/sint slot masks
 * in `uint8_t`, so `1u << slot` for slots 8..15 was truncated to nothing. That
 * fed two separate tables, and this probe covers the second one:
 *
 *   - `texture_sample_types[i]` stayed Float instead of Depth, and
 *   - `sampler_types[i]` stayed Filtering instead of Comparison.
 *
 * A shadow sampler above the eighth slot therefore reached WebGPU declared as
 * an ordinary filtering sample of a float texture, over a view whose format is
 * a depth one. That is not a legal pairing, so the bind group is invalid --
 * and the cost is not confined to the high slots. The *whole draw* is dropped:
 * with the bug present every pixel here reads back the clear value, low slots
 * included. A renderer that declares nine or more comparison samplers renders
 * nothing at all, which is why this is worth its own probe rather than a note
 * on the other one.
 *
 * Twelve samplers, not two, and that is the point. A sampler's slot is its
 * compacted driver_location, not the texture unit glUniform1i names, so
 * declaring two and binding one to unit 10 puts it at slot 1 and never reaches
 * the truncated bits. The first version of this probe did exactly that and
 * passed with the bug present -- the same trap as the u_vbuf layouts. Prove a
 * probe fails against the defect before trusting it to guard against it.
 *
 * Slots 0..7 are the control. If both cases fail the shader or the harness is
 * wrong; the signature of this bug is that everything fails together, so the
 * control failing alone still means something else.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>

/* Twelve, not two. A sampler's *slot* is its compacted driver_location, not
 * the texture unit glUniform1i names, so declaring two and binding one to
 * unit 10 puts it at slot 1 and never reaches the truncated bits at all --
 * the first version of this probe did exactly that and passed with the bug
 * present. Only declaring twelve samplers places four of them above seven. */
#define N 12
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
static void depth_texture(int unit) {
    GLuint t = 0;
    glActiveTexture(GL_TEXTURE0 + unit);
    glGenTextures(1, &t);
    glBindTexture(GL_TEXTURE_2D, t);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_DEPTH_COMPONENT32F, 1, 1);
    const float half = 0.5f;
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 1, 1, GL_DEPTH_COMPONENT,
                    GL_FLOAT, &half);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_MODE,
                    GL_COMPARE_REF_TO_TEXTURE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_FUNC, GL_LEQUAL);
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
    const EGLint cx[] = { EGL_CONTEXT_MAJOR_VERSION, 3,
                          EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE };
    const EGLint sa[] = { EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE };
    EGLContext c = eglCreateContext(d, cfg, EGL_NO_CONTEXT, cx);
    EGLSurface s = eglCreatePbufferSurface(d, cfg, sa);
    if (c == EGL_NO_CONTEXT || s == EGL_NO_SURFACE ||
        !eglMakeCurrent(d, s, s, c)) return 2;

    for (int i = 0; i < N; ++i) depth_texture(i);

    /* 2*N points. The x coordinate picks the sampler and the half of the row
     * picks the reference: 0.25 is nearer than the stored 0.5 and passes
     * GL_LEQUAL, 0.75 is further and fails. A probe that only ever expects
     * 1.0 cannot tell a working comparison from a sampler that returns one
     * unconditionally. */
    char vs[512], fs[4096];
    snprintf(vs, sizeof vs,
        "#version 330 core\n"
        "flat out int v_id;\n"
        "void main(){\n"
        "  gl_Position = vec4((2.0*(float(gl_VertexID)+0.5)/%d.0)-1.0,\n"
        "                     0.0, 0.0, 1.0);\n"
        "  v_id = gl_VertexID;\n}\n", 2 * N);
    int at = snprintf(fs, sizeof fs,
        "#version 330 core\n"
        "uniform sampler2DShadow s[%d];\n"
        "flat in int v_id;\n"
        "out uint outc;\n"
        "void main(){\n"
        "  float ref = (v_id < %d) ? 0.25 : 0.75;\n"
        "  int which = v_id %% %d;\n"
        "  float v = 0.0;\n", N, N, N);
    /* Literal indices: GLSL 3.30 will not index a sampler array with a value
     * the compiler cannot fold. */
    for (int i = 0; i < N; ++i)
        at += snprintf(fs + at, sizeof fs - at,
            "  if (which == %d) v = texture(s[%d], vec3(0.5, 0.5, ref));\n",
            i, i);
    snprintf(fs + at, sizeof fs - at,
        "  outc = uint(v * 255.0 + 0.5);\n}\n");

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
        char nm[32]; snprintf(nm, sizeof nm, "s[%d]", i);
        glUniform1i(glGetUniformLocation(p, nm), i);
    }

    GLuint rt = 0, fbo = 0;
    glGenTextures(1, &rt);
    glActiveTexture(GL_TEXTURE0 + N);
    glBindTexture(GL_TEXTURE_2D, rt);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, 2 * N, 1);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, rt, 0);
    glViewport(0, 0, 2 * N, 1);
    /* 0xffff marks a pixel nothing wrote, so a dropped draw is not read as a
     * comparison that returned zero. */
    const GLuint clear[4] = {0xffffu, 0xffffu, 0xffffu, 0xffffu};
    glClearBufferuiv(GL_COLOR, 0, clear);

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glDrawArrays(GL_POINTS, 0, 2 * N);

    GLuint got[2 * N];
    for (int i = 0; i < 2 * N; ++i) got[i] = 0;
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    glReadPixels(0, 0, 2 * N, 1, GL_RED_INTEGER, GL_UNSIGNED_INT, got);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO near:");
    for (int i = 0; i < N; ++i) fprintf(stderr, " %u", got[i]);
    fprintf(stderr, "  far:");
    for (int i = 0; i < N; ++i) fprintf(stderr, " %u", got[N + i]);
    fprintf(stderr, " err=0x%04x\n", (unsigned)error);

    const int ok = linked && error == GL_NO_ERROR;
    int low = 1, high = 1;
    for (int i = 0; i < 8; ++i)
        if (got[i] != 255 || got[N + i] != 0) low = 0;
    for (int i = 8; i < N; ++i)
        if (got[i] != 255 || got[N + i] != 0) high = 0;
    result("slots_0_7_control", ok && low);
    result("slots_8_11_shadow", ok && high);
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
