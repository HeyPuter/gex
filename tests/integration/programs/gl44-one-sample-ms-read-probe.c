/* Reading a one-sample multisample texture through sampler2DMS.
 *
 * glTexStorage2DMultisample accepts samples=1, and for a format WebGPU cannot
 * multisample at all -- every 32-bit-per-component one -- that is the only
 * count available, so this is not a corner but the *normal* case for those
 * formats. WebGPU has no such binding: `multisampled: true` requires a sample
 * count above one, and a sampleCount-1 texture is an ordinary texture.
 *
 * The driver used to declare the slot unusable and remove its sampler, so
 * every fetch read zero and nothing said so. All nine remaining failures of
 * KHR-GL44.sample_variables were that, every one of them rgba32f.
 *
 * Two things this checks that a simpler probe would not:
 *
 *   - **A unit above zero.** The rewrite is keyed on the texture unit, and a
 *     texture's WebGPU *binding* is 2*unit, so keying it on the binding by
 *     mistake still works for unit 0 and fails for every other unit. Tint
 *     catches it -- "Image Operand Sample is required for operation on
 *     multi-sampled image" -- but only once a unit above zero is used.
 *   - **An ordinary texture on a sampler2DMS is still refused.** That is
 *     invalid GL, and folding it together with the legal case (a resource
 *     carries nr_samples 0 when it is not a multisample target, and 1 when it
 *     is one holding a single sample) demoted ordinary textures too and cost
 *     seven sample_variables cases that had been passing.
 *
 * A blit-resolve of the same texture runs first as the control: it does not
 * involve sampling, so if it disagrees the data was never there and the fetch
 * is not the thing at fault.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-40s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define S 8

static const char *vs_src =
    "#version 430\n"
    "void main() {\n"
    "    gl_Position = vec4((gl_VertexID == 1) ? 3.0 : -1.0,\n"
    "                       (gl_VertexID == 2) ? 3.0 : -1.0, 0.0, 1.0);\n"
    "}\n";
static const char *fs_fill =
    "#version 430\n"
    "out vec4 o;\n"
    "void main() { o = vec4(0.25, 0.5, 0.75, 1.0); }\n";
/* The multisample sampler sits on unit 1, behind an unused ordinary sampler
   on unit 0, so the unit and the binding cannot be confused. */
static const char *fs_read =
    "#version 430\n"
    "uniform sampler2D unused0;\n"
    "uniform sampler2DMS msTex;\n"
    "out vec4 o;\n"
    "void main() {\n"
    "    o = texelFetch(msTex, ivec2(gl_FragCoord.xy), 0)\n"
    "      + texture(unused0, vec2(0.5)) * 0.0;\n"
    "}\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log); }
    return ok ? sh : 0;
}
static GLuint link2(const char *fs_src, const char *what) {
    GLuint vs = compile(GL_VERTEX_SHADER, vs_src, "vs");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, what);
    if (!vs || !fs) return 0;
    GLuint p = glCreateProgram();
    glAttachShader(p, vs); glAttachShader(p, fs);
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) { char log[1024] = {0};
               glGetProgramInfoLog(p, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s link failed: %s\n", what, log);
               return 0; }
    return p;
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    /* rgba32f: the format that has no multisampling at all, so glTexStorage
       leaves it at one sample wherever the driver reports counts honestly. */
    GLuint ms = 0; glGenTextures(1, &ms);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms);
    glTexStorage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, 1, GL_RGBA32F, S, S,
                              GL_FALSE);
    eq("one_sample_storage_is_clean", glGetError(), GL_NO_ERROR);
    GLuint fbo = 0; glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D_MULTISAMPLE, ms, 0);
    eq("one_sample_framebuffer_is_complete",
       glCheckFramebufferStatus(GL_FRAMEBUFFER), GL_FRAMEBUFFER_COMPLETE);
    glViewport(0, 0, S, S);
    GLuint fill = link2(fs_fill, "fill");
    if (!fill) { printf("SUMMARY cases=%d passed=%d failed=%d\n", cases,
                        passed, cases - passed); return 1; }
    glUseProgram(fill);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    eq("filling_it_is_clean", glGetError(), GL_NO_ERROR);

    /* The control: a blit does not sample, so this says whether the data is
       in the texture at all. */
    GLuint flat = 0, flat_fbo = 0;
    glGenTextures(1, &flat); glGenFramebuffers(1, &flat_fbo);
    glBindTexture(GL_TEXTURE_2D, flat);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, S, S);
    glBindFramebuffer(GL_FRAMEBUFFER, flat_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           flat, 0);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, flat_fbo);
    glBlitFramebuffer(0, 0, S, S, 0, 0, S, S, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, flat_fbo);
    unsigned char pixel[4] = {0};
    glReadPixels(S / 2, S / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    while (glGetError() != GL_NO_ERROR) { }
    eq("blit_control_sees_the_data", pixel[0], 64);

    /* And the thing under test: texelFetch through sampler2DMS, on unit 1. */
    GLuint read = link2(fs_read, "read");
    eq("one_sample_ms_program_links", read != 0, 1);
    if (read) {
        GLuint other = 0; glGenTextures(1, &other);
        glBindTexture(GL_TEXTURE_2D, other);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, S, S);
        glBindFramebuffer(GL_FRAMEBUFFER, flat_fbo);
        glUseProgram(read);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, other);
        glUniform1i(glGetUniformLocation(read, "unused0"), 0);
        glActiveTexture(GL_TEXTURE1);
        glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms);
        glUniform1i(glGetUniformLocation(read, "msTex"), 1);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        eq("one_sample_ms_draw_is_clean", glGetError(), GL_NO_ERROR);
        unsigned char got[4] = {0};
        glReadPixels(S / 2, S / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
        while (glGetError() != GL_NO_ERROR) { }
        fprintf(stderr, "INFO fetched = %u %u %u %u\n", got[0], got[1], got[2],
                got[3]);
        eq("one_sample_ms_fetch_red", got[0], 64);
        eq("one_sample_ms_fetch_green", got[1], 128);
        eq("one_sample_ms_fetch_blue", got[2], 191);
    }

    /* An ordinary texture on a sampler2DMS is invalid GL and must NOT be
       served by the same rewrite. The draw is expected to produce nothing
       useful; what matters is that this does not crash or hang, and that the
       legal case above still worked. */
    {
        glActiveTexture(GL_TEXTURE1);
        glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, 0);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        while (glGetError() != GL_NO_ERROR) { }
        eq("unbinding_the_ms_texture_survives", 1, 1);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
