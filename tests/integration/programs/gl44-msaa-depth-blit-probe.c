#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum object_kind { OBJECT_TEXTURE, OBJECT_RENDERBUFFER, OBJECT_DEFAULT };

static GLenum internal_format = GL_DEPTH_COMPONENT24;
static const char *format_name = "depth24";
static int has_stencil = 0;
static const GLfloat depths[4] = {0.125f, 0.375f, 0.625f, 0.875f};

struct format_case {
    GLenum internal_format;
    const char *name;
    int has_stencil;
};

static const struct format_case format_cases[] = {
    {GL_DEPTH_COMPONENT24, "depth24", 0},
    {GL_DEPTH_COMPONENT16, "depth16", 0},
    {GL_DEPTH_COMPONENT32F, "depth32f", 0},
    {GL_DEPTH24_STENCIL8, "depth24_stencil8", 1},
    {GL_DEPTH32F_STENCIL8, "depth32f_stencil8", 1},
    {GL_DEPTH_COMPONENT32, "depth32", 0},
};

static void select_format(const struct format_case *format)
{
    internal_format = format->internal_format;
    format_name = format->name;
    has_stencil = format->has_stencil;
}

static GLuint make_depth(enum object_kind kind, int multisampled)
{
    GLuint name = 0;
    if (kind == OBJECT_TEXTURE) {
        glGenTextures(1, &name);
        if (multisampled) {
            glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, name);
            glTexStorage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, 4,
                                      internal_format, 16, 16, GL_TRUE);
        } else {
            glBindTexture(GL_TEXTURE_2D, name);
            glTexStorage2D(GL_TEXTURE_2D, 1, internal_format, 16, 16);
        }
    } else {
        glGenRenderbuffers(1, &name);
        glBindRenderbuffer(GL_RENDERBUFFER, name);
        if (multisampled)
            glRenderbufferStorageMultisample(GL_RENDERBUFFER, 4, internal_format, 16, 16);
        else
            glRenderbufferStorage(GL_RENDERBUFFER, internal_format, 16, 16);
    }
    return name;
}

static void attach_depth(GLenum target, enum object_kind kind, GLuint name, int multisampled)
{
    const GLenum attachment = has_stencil ? GL_DEPTH_STENCIL_ATTACHMENT : GL_DEPTH_ATTACHMENT;
    if (kind == OBJECT_TEXTURE)
        glFramebufferTexture2D(target, attachment,
                               multisampled ? GL_TEXTURE_2D_MULTISAMPLE : GL_TEXTURE_2D,
                               name, 0);
    else
        glFramebufferRenderbuffer(target, attachment, GL_RENDERBUFFER, name);
}

static const char *kind_name(enum object_kind kind)
{
    return kind == OBJECT_TEXTURE ? "tex" :
           (kind == OBJECT_RENDERBUFFER ? "rb" : "default");
}

static int run_case(enum object_kind source_kind, enum object_kind dest_kind)
{
    GLuint source = make_depth(source_kind, 1);
    GLuint dest = dest_kind == OBJECT_DEFAULT ? 0 : make_depth(dest_kind, 0);
    GLuint source_fbo = 0, dest_fbo = 0;
    glGenFramebuffers(1, &source_fbo);
    if (dest_kind != OBJECT_DEFAULT) glGenFramebuffers(1, &dest_fbo);

    glBindFramebuffer(GL_FRAMEBUFFER, source_fbo);
    attach_depth(GL_FRAMEBUFFER, source_kind, source, 1);
    GLenum source_status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    GLint sample_buffers = -1, samples = -1;
    glGetIntegerv(GL_SAMPLE_BUFFERS, &sample_buffers);
    glGetIntegerv(GL_SAMPLES, &samples);
    printf("SAMPLES format=%s src=%s buffers=%d samples=%d expected=1,4\n",
           format_name, kind_name(source_kind), sample_buffers, samples);

    glBindFramebuffer(GL_FRAMEBUFFER, dest_fbo);
    if (dest_kind != OBJECT_DEFAULT) {
        attach_depth(GL_FRAMEBUFFER, dest_kind, dest, 0);
    }
    GLenum dest_status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    GLenum setup_error = glGetError();
    if (source_status != GL_FRAMEBUFFER_COMPLETE || sample_buffers != 1 || samples != 4 ||
        dest_status != GL_FRAMEBUFFER_COMPLETE || setup_error != GL_NO_ERROR) {
        printf("CASE format=%s src=%s dst=%s setup=0x%04x,0x%04x error=0x%04x result=FAIL\n",
               format_name, kind_name(source_kind), kind_name(dest_kind),
               source_status, dest_status, setup_error);
        return 1;
    }

    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, source_fbo);
    glEnable(GL_SCISSOR_TEST);
    glDepthMask(GL_TRUE);
    for (int quadrant = 0; quadrant < 4; ++quadrant) {
        glScissor((quadrant & 1) ? 8 : 0, (quadrant & 2) ? 8 : 0, 8, 8);
        glClearDepth(depths[quadrant]);
        glClear(GL_DEPTH_BUFFER_BIT);
    }
    if (has_stencil) {
        for (int quadrant = 0; quadrant < 4; ++quadrant) {
            glScissor((quadrant & 1) ? 8 : 0, (quadrant & 2) ? 8 : 0, 8, 8);
            glClearStencil(quadrant + 1);
            glClear(GL_STENCIL_BUFFER_BIT);
        }
    }
    glDisable(GL_SCISSOR_TEST);

    /* CTS initializes both destination aspects before the blit. This proves
     * that the operation replaces clear source bits instead of preserving
     * stale destination state. */
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dest_fbo);
    glClearDepth(0.25);
    glClearStencil(0x5a);
    glClear(GL_DEPTH_BUFFER_BIT | (has_stencil ? GL_STENCIL_BUFFER_BIT : 0));

    glBindFramebuffer(GL_READ_FRAMEBUFFER, source_fbo);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dest_fbo);
    glBlitFramebuffer(0, 0, 16, 16, 0, 0, 16, 16,
                      GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT |
                          (has_stencil ? GL_STENCIL_BUFFER_BIT : 0),
                      GL_NEAREST);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, dest_fbo);

    int failed = 0;
    for (int quadrant = 0; quadrant < 4; ++quadrant) {
        GLfloat value = -1.0f;
        glReadPixels((quadrant & 1) ? 12 : 4, (quadrant & 2) ? 12 : 4,
                     1, 1, GL_DEPTH_COMPONENT, GL_FLOAT, &value);
        printf("DEPTH format=%s src=%s dst=%s quadrant=%d value=%.6f expected=%.6f\n",
               format_name, kind_name(source_kind), kind_name(dest_kind), quadrant,
               value, depths[quadrant]);
        if (fabsf(value - depths[quadrant]) > 0.002f) failed = 1;
        if (has_stencil) {
            GLuint stencil = 0;
            glReadPixels((quadrant & 1) ? 12 : 4, (quadrant & 2) ? 12 : 4,
                         1, 1, GL_STENCIL_INDEX, GL_UNSIGNED_INT, &stencil);
            printf("STENCIL format=%s src=%s dst=%s quadrant=%d value=%u expected=%d\n",
                   format_name, kind_name(source_kind), kind_name(dest_kind), quadrant,
                   stencil, quadrant + 1);
            if (stencil != (GLuint)(quadrant + 1)) failed = 1;
        }
    }
    GLenum error = glGetError();
    printf("CASE format=%s src=%s dst=%s error=0x%04x result=%s\n",
           format_name, kind_name(source_kind), kind_name(dest_kind), error,
           !failed && error == GL_NO_ERROR ? "PASS" : "FAIL");

    glDeleteFramebuffers(1, &source_fbo);
    if (dest_kind != OBJECT_DEFAULT) glDeleteFramebuffers(1, &dest_fbo);
    if (source_kind == OBJECT_TEXTURE) glDeleteTextures(1, &source);
    else glDeleteRenderbuffers(1, &source);
    if (dest_kind == OBJECT_TEXTURE) glDeleteTextures(1, &dest);
    else if (dest_kind == OBJECT_RENDERBUFFER) glDeleteRenderbuffers(1, &dest);
    return failed || error != GL_NO_ERROR;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    EGLConfig config = NULL;
    EGLint count = 0;
    const EGLint config_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                   EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE};
    eglChooseConfig(display, config_attrs, &config, 1, &count);
    const EGLint context_attrs[] = {EGL_CONTEXT_MAJOR_VERSION, 4,
                                    EGL_CONTEXT_MINOR_VERSION, 4, EGL_NONE};
    const EGLint surface_attrs[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attrs);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attrs);
    if (count < 1 || context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    if (argc >= 2 && strcmp(argv[1], "all") == 0) {
        int failed = 0;
        int cases = 0;
        const enum object_kind kinds[] = {OBJECT_TEXTURE, OBJECT_RENDERBUFFER};
        for (size_t format = 0; format < sizeof(format_cases) / sizeof(format_cases[0]); ++format) {
            select_format(&format_cases[format]);
            for (size_t source = 0; source < sizeof(kinds) / sizeof(kinds[0]); ++source) {
                for (size_t dest = 0; dest < sizeof(kinds) / sizeof(kinds[0]); ++dest) {
                    failed += run_case(kinds[source], kinds[dest]);
                    ++cases;
                }
            }
        }
        printf("SUMMARY cases=%d failed=%d\n", cases, failed);
        return failed ? 1 : 0;
    }

    if (argc >= 4) {
        for (size_t format = 0; format < sizeof(format_cases) / sizeof(format_cases[0]); ++format) {
            if (strcmp(argv[3], format_cases[format].name) == 0) {
                select_format(&format_cases[format]);
                break;
            }
        }
    }

    enum object_kind source = argc >= 2 && strcmp(argv[1], "rb") == 0
                                  ? OBJECT_RENDERBUFFER : OBJECT_TEXTURE;
    enum object_kind dest = argc >= 3 && strcmp(argv[2], "rb") == 0
                                ? OBJECT_RENDERBUFFER
                                : (argc >= 3 && strcmp(argv[2], "default") == 0
                                       ? OBJECT_DEFAULT : OBJECT_TEXTURE);
    int failed = run_case(source, dest);
    printf("SUMMARY cases=1 failed=%d\n", failed);
    return failed ? 1 : 0;
}
