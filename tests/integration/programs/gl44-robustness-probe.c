/* KHR_robustness' bounded queries, and ARB_get_texture_sub_image.
 *
 * What these exist for is the refusal. glGetnUniformfv with a buffer too small
 * must report GL_INVALID_OPERATION and write nothing -- an implementation that
 * truncates instead, or writes what fits, has defeated the entire purpose,
 * because the caller cannot tell the difference from a correct short answer.
 * So every case here checks the guard byte after the buffer as well as the
 * value in it.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include "gl-procs.h"
#include <stdio.h>
#include <string.h>

static int cases = 0, failures = 0;
static void report(const char* name, int ok, const char* detail) {
    ++cases;
    if (!ok) ++failures;
    printf("CASE %s %s%s%s\n", name, ok ? "PASS" : "FAIL",
           detail && *detail ? " -- " : "", detail ? detail : "");
}
#define NEED(c, m) do { if (!(c)) { fprintf(stderr, "robustness failed: %s\n", m); return __LINE__; } } while (0)

static const char* vs_source =
    "#version 400\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char* fs_source =
    "#version 400\n"
    "uniform vec4 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = tint; }\n";

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    NEED(eglInitialize(display, NULL, NULL), "eglInitialize");
    NEED(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL; EGLint config_count = 0;
    NEED(eglChooseConfig(display, config_attributes, &config, 1, &config_count),
         "eglChooseConfig");
    NEED(config_count >= 1 && config != NULL, "no pbuffer config");
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    NEED(context != EGL_NO_CONTEXT && surface != EGL_NO_SURFACE, "context/surface");
    NEED(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");
    /* Resolved, not linked: see gl-procs.h. Without this the probe cannot run
       against our own libGL at all. */
    if (gex_load_procs() != 0) return 3;

    /* A reset status is the one call an application polls per frame under
     * robustness. GL_NO_ERROR is the answer when nothing has been lost. */
    {
        const GLenum status = glGetGraphicsResetStatus();
        char detail[64];
        snprintf(detail, sizeof detail, "status 0x%x", (unsigned)status);
        report("graphics_reset_status", status == GL_NO_ERROR, detail);
    }

    GLuint program = glCreateProgram();
    GLuint v = glCreateShader(GL_VERTEX_SHADER), f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(v, 1, &vs_source, NULL); glCompileShader(v);
    glShaderSource(f, 1, &fs_source, NULL); glCompileShader(f);
    glAttachShader(program, v); glAttachShader(program, f);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    NEED(linked, "link");
    glUseProgram(program);
    const GLint tint = glGetUniformLocation(program, "tint");
    NEED(tint >= 0, "uniform location");
    glUniform4f(tint, 0.25f, 0.5f, 0.75f, 1.0f);

    /* Big enough: the value arrives. */
    {
        float got[8];
        memset(got, 0, sizeof got);
        glGetnUniformfv(program, tint, (GLsizei)(4 * sizeof(float)), got);
        char detail[96];
        snprintf(detail, sizeof detail, "%.2f,%.2f,%.2f,%.2f", got[0], got[1],
                 got[2], got[3]);
        report("getn_uniform_fits",
               got[0] == 0.25f && got[1] == 0.5f && got[2] == 0.75f &&
                   got[3] == 1.0f, detail);
    }

    /* Too small: nothing may be written, not even the part that would fit.
     * The sentinel is what makes this a real test -- a truncating
     * implementation passes any check that only looks at the first element. */
    {
        float got[8];
        for (int i = 0; i < 8; ++i) got[i] = -7.0f;
        while (glGetError() != GL_NO_ERROR) { }
        glGetnUniformfv(program, tint, (GLsizei)(2 * sizeof(float)), got);
        const GLenum err = glGetError();
        char detail[112];
        snprintf(detail, sizeof detail, "err=0x%x buffer=%.2f,%.2f,%.2f,%.2f",
                 (unsigned)err, got[0], got[1], got[2], got[3]);
        /* Both halves. The untouched buffer is the safety property; the
         * error is what lets the caller find out -- refusing silently means
         * an application checks glGetError, sees success, and reads the
         * buffer it never had filled. */
        report("getn_uniform_refuses_short_buffer",
               err == GL_INVALID_OPERATION && got[0] == -7.0f &&
                   got[1] == -7.0f && got[2] == -7.0f, detail);
    }

    /* The same rule for pixels. */
    {
        GLuint colour = 0, fbo = 0;
        glGenRenderbuffers(1, &colour);
        glBindRenderbuffer(GL_RENDERBUFFER, colour);
        glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 4, 4);
        glGenFramebuffers(1, &fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                  GL_RENDERBUFFER, colour);
        const GLfloat green[4] = {0.0f, 1.0f, 0.0f, 1.0f};
        glClearBufferfv(GL_COLOR, 0, green);

        unsigned char big[4 * 4 * 4];
        memset(big, 0xAB, sizeof big);
        glReadnPixels(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, (GLsizei)sizeof big,
                      big);
        const int fits_ok = big[0] == 0 && big[1] == 255 && big[2] == 0;

        unsigned char small[4 * 4 * 4];
        memset(small, 0xAB, sizeof small);
        while (glGetError() != GL_NO_ERROR) { }
        glReadnPixels(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, 16, small);
        const GLenum read_err = glGetError();
        const int refused_ok = read_err == GL_INVALID_OPERATION &&
                               small[0] == 0xAB && small[1] == 0xAB;
        char detail[96];
        snprintf(detail, sizeof detail, "fits=%d refused=%d err=0x%x first=%u",
                 fits_ok, refused_ok, (unsigned)read_err, small[0]);
        report("readn_pixels", fits_ok && refused_ok, detail);
    }

    /* A region of a level, which is what ARB_get_texture_sub_image adds. */
    {
        GLuint texture = 0;
        glGenTextures(1, &texture);
        glBindTexture(GL_TEXTURE_2D, texture);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 2, 2);
        static const unsigned char texels[16] = {
            10, 20, 30, 255,    40, 50, 60, 255,
            70, 80, 90, 255,    100, 110, 120, 255,
        };
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 2, 2, GL_RGBA,
                        GL_UNSIGNED_BYTE, texels);
        unsigned char one[8];
        memset(one, 0, sizeof one);
        glGetTextureSubImage(texture, 0, 1, 0, 0, 1, 1, 1, GL_RGBA,
                             GL_UNSIGNED_BYTE, (GLsizei)4, one);
        char detail[96];
        snprintf(detail, sizeof detail, "texel(1,0) = %u,%u,%u,%u", one[0],
                 one[1], one[2], one[3]);
        report("get_texture_sub_image",
               one[0] == 40 && one[1] == 50 && one[2] == 60, detail);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures,
           failures);
    return failures ? 1 : 0;
}
