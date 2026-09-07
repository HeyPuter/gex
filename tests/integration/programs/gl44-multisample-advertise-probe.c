/* Whatever glGetInternalformativ advertises for GL_SAMPLES must actually work.
 *
 * KHR-GL44.texture_view fails two cases on
 *
 *   glTexStorage2DMultisample() call failed:
 *   glGetError() returned GL_INVALID_OPERATION
 *
 * and the sample count it passes is not one it invented -- it asked the driver
 * first, with glGetInternalformativ(GL_RGBA32F, GL_SAMPLES), and used the
 * answer. So the query and the allocator disagree, which is the whole defect:
 * an application that asks what it may have and is then refused it has no way
 * forward. WebGPU offers only 4x multisampling and excludes the 32-bit-per-
 * component formats from it, so the honest answer for rgba32f is *no* sample
 * counts rather than a count that cannot be allocated.
 *
 * Each format is checked both ways round -- every advertised count must be
 * allocatable, and a format advertising nothing must refuse cleanly rather
 * than crash -- because a driver can fail this by being too generous or by
 * being silently wrong, and only the pair distinguishes them.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include "gl-procs.h"
#include <stdio.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-44s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

struct format { GLenum token; const char *name; };
static const struct format formats[] = {
    { GL_RGBA8,             "rgba8"   },
    { GL_RGBA16F,           "rgba16f" },
    { GL_RGBA32F,           "rgba32f" },
    { GL_R32F,              "r32f"    },
    { GL_RG32F,             "rg32f"   },
    { GL_RGBA32UI,          "rgba32ui"},
    { GL_DEPTH24_STENCIL8,  "d24s8"   },
};

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
    /* glGetInternalformativ is absent from Mesa's non-glvnd libGL and has
       to be resolved; see gl-procs.h. Without this the probe dies at load
       against our own build and never runs against the driver at all. */
    /* Returns how many it could *not* resolve, so zero is success. */
    if (gex_load_procs() != 0) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 3;
    }

    GLint global_max = 0;
    glGetIntegerv(GL_MAX_SAMPLES, &global_max);
    fprintf(stderr, "INFO GL_MAX_SAMPLES=%d\n", global_max);
    drain();

    for (unsigned f = 0; f < sizeof formats / sizeof formats[0]; ++f) {
        const struct format *fmt = &formats[f];
        GLint count = 0;
        glGetInternalformativ(GL_TEXTURE_2D_MULTISAMPLE, fmt->token,
                              GL_NUM_SAMPLE_COUNTS, 1, &count);
        if (drain() != GL_NO_ERROR) count = -1;
        GLint samples[32] = {0};
        const GLint want = count > 0 ? (count < 32 ? count : 32) : 0;
        if (want > 0) {
            glGetInternalformativ(GL_TEXTURE_2D_MULTISAMPLE, fmt->token,
                                  GL_SAMPLES, want, samples);
            drain();
        }
        fprintf(stderr, "INFO %-9s num_sample_counts=%d samples=", fmt->name,
                count);
        for (GLint i = 0; i < want; ++i)
            fprintf(stderr, "%d%s", samples[i], i + 1 < want ? "," : "");
        fprintf(stderr, "%s\n", want ? "" : "(none)");

        /* Every advertised count must allocate. Highest first, which is what
           the CTS uses and the one most likely to be over-promised. */
        int refused = 0;
        for (GLint i = 0; i < want; ++i) {
            GLuint tex = 0;
            glGenTextures(1, &tex);
            glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, tex);
            glTexStorage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, samples[i],
                                      fmt->token, 16, 16, GL_TRUE);
            const GLenum error = drain();
            if (error != GL_NO_ERROR) {
                ++refused;
                fprintf(stderr, "INFO   %s x%d refused 0x%04x\n", fmt->name,
                        samples[i], (unsigned)error);
            }
            glDeleteTextures(1, &tex);
            drain();
        }
        char name[80];
        snprintf(name, sizeof name, "%s_every_advertised_count_allocates",
                 fmt->name);
        eq(name, refused, 0);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
