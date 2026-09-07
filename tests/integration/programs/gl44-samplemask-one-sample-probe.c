/* gl_SampleMask on a one-sample *multisample* framebuffer.
 *
 * GL performs the multisample fragment operations -- where a fragment
 * shader's gl_SampleMask is ANDed into the coverage -- when SAMPLE_BUFFERS is
 * one, and SAMPLE_BUFFERS is one whenever the framebuffer's attachments are
 * multisample. That is a different question from how many samples they have:
 * a texture made with glTexImage2DMultisample and samples=1 is multisample,
 * has SAMPLE_BUFFERS 1, and must honour the mask. An ordinary texture is not,
 * and must ignore it.
 *
 * The driver here keys its mask-neutralising fragment variant on the
 * framebuffer being single-sampled, which conflates the two, and every one of
 * KHR-GL44.sample_variables' 37 failures is at samples_1 -- nine in each of
 * rgba8, rgba8i, rgba8ui and rgba32f, and nothing at any other sample count.
 * This is that distinction, in three cases, so a driver that gets one right
 * and the other wrong says which.
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
    fprintf(stderr, "INFO %-46s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

static const char *vs_src =
    "#version 400\n"
    "void main() {\n"
    "    gl_Position = vec4((gl_VertexID == 1) ? 3.0 : -1.0,\n"
    "                       (gl_VertexID == 2) ? 3.0 : -1.0, 0.0, 1.0);\n"
    "}\n";
/* Writes zero, so every sample it covers is masked away. */
static const char *fs_src =
    "#version 400\n"
    "out vec4 colour;\n"
    "void main() {\n"
    "    gl_SampleMask[0] = 0;\n"
    "    colour = vec4(1.0, 0.0, 0.0, 1.0);\n"
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

/* Draw the masked triangle into a framebuffer of `samples` (0 meaning an
   ordinary, non-multisample texture) and return the red channel that
   survived. 255 means the fragment was kept, 0 means it was masked away. */
static int drew_through(GLuint program, int samples, const char *label) {
    GLuint tex = 0, fbo = 0;
    glGenTextures(1, &tex);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    if (samples > 0) {
        glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, tex);
        glTexImage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, samples, GL_RGBA8,
                                16, 16, GL_TRUE);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D_MULTISAMPLE, tex, 0);
    } else {
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 16, 16);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, tex, 0);
    }
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "INFO %s: framebuffer incomplete\n", label);
        return -1;
    }
    GLint reported = -1, buffers = -1;
    glGetIntegerv(GL_SAMPLES, &reported);
    glGetIntegerv(GL_SAMPLE_BUFFERS, &buffers);
    fprintf(stderr, "INFO %s: GL_SAMPLES=%d GL_SAMPLE_BUFFERS=%d\n", label,
            reported, buffers);

    glViewport(0, 0, 16, 16);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(program);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    if (drain() != GL_NO_ERROR) {
        fprintf(stderr, "INFO %s: draw raised an error\n", label);
        return -1;
    }

    unsigned char pixel[4] = {0};
    if (samples > 0) {
        /* Resolve into an ordinary framebuffer to read it back. */
        GLuint flat = 0, flat_fbo = 0;
        glGenTextures(1, &flat); glGenFramebuffers(1, &flat_fbo);
        glBindTexture(GL_TEXTURE_2D, flat);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 16, 16);
        glBindFramebuffer(GL_FRAMEBUFFER, flat_fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, flat, 0);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, flat_fbo);
        glBlitFramebuffer(0, 0, 16, 16, 0, 0, 16, 16, GL_COLOR_BUFFER_BIT,
                          GL_NEAREST);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, flat_fbo);
        glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    } else {
        glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    }
    drain();
    return pixel[0];
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

    GLuint vs = compile(GL_VERTEX_SHADER, vs_src, "vs");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !fs) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log); }
    eq("samplemask_program_links", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n", cases,
                          passed, cases - passed); return 1; }
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    /* An ordinary framebuffer: SAMPLE_BUFFERS is zero, the mask is ignored,
       the fragment survives. */
    eq("plain_framebuffer_ignores_the_mask",
       drew_through(program, 0, "plain"), 255);

    /* One-sample multisample: SAMPLE_BUFFERS is one, so the mask applies and
       the fragment is masked away. This is the distinction the driver's
       single-sample predicate misses. */
    eq("one_sample_multisample_honours_the_mask",
       drew_through(program, 1, "1x multisample"), 0);

    /* And the ordinary multisample case, for contrast. */
    eq("four_sample_multisample_honours_the_mask",
       drew_through(program, 4, "4x multisample"), 0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
