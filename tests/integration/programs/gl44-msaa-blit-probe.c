#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum object_kind { OBJECT_TEXTURE, OBJECT_RENDERBUFFER, OBJECT_DEFAULT };

static GLenum probe_internal_format = GL_RGBA8;
static const char *probe_format_name = "rgba8";

static const GLubyte colors[4][4] = {
    { 255,   0,   0, 255 },
    {   0, 255,   0, 255 },
    {   0,   0, 255, 255 },
    { 255, 255,   0, 255 },
};

static void fail_gl(const char *where)
{
    fprintf(stderr, "FAIL %s gl=0x%04x\n", where, glGetError());
    exit(2);
}

static GLuint make_color_object(enum object_kind kind, int multisampled)
{
    GLuint name = 0;
    if (kind == OBJECT_TEXTURE) {
        glGenTextures(1, &name);
        if (multisampled) {
            glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, name);
            glTexStorage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, 4,
                                      probe_internal_format, 16, 16, GL_TRUE);
        } else {
            glBindTexture(GL_TEXTURE_2D, name);
            glTexStorage2D(GL_TEXTURE_2D, 1, probe_internal_format, 16, 16);
        }
    } else {
        glGenRenderbuffers(1, &name);
        glBindRenderbuffer(GL_RENDERBUFFER, name);
        if (multisampled)
            glRenderbufferStorageMultisample(GL_RENDERBUFFER, 4,
                                             probe_internal_format, 16, 16);
        else
            glRenderbufferStorage(GL_RENDERBUFFER, probe_internal_format, 16, 16);
    }
    if (glGetError() != GL_NO_ERROR)
        fail_gl("allocate");
    return name;
}

static void attach_color(GLenum target, GLenum attachment,
                         enum object_kind kind, GLuint name, int multisampled)
{
    if (kind == OBJECT_TEXTURE)
        glFramebufferTexture2D(target, attachment,
                               multisampled ? GL_TEXTURE_2D_MULTISAMPLE
                                            : GL_TEXTURE_2D,
                               name, 0);
    else
        glFramebufferRenderbuffer(target, attachment, GL_RENDERBUFFER, name);
}

static int close_pixel(const GLubyte *actual, const GLubyte *expected)
{
    for (int component = 0; component < 4; ++component) {
        int delta = (int)actual[component] - (int)expected[component];
        if (delta < -1 || delta > 1)
            return 0;
    }
    return 1;
}

static void expected_pixel(int quadrant, GLubyte expected[4])
{
    memcpy(expected, colors[quadrant], 4);
    if (probe_internal_format == GL_R8 || probe_internal_format == GL_R16F) {
        expected[1] = expected[2] = 0;
        expected[3] = 255;
    } else if (probe_internal_format == GL_RG8 || probe_internal_format == GL_RG16F) {
        expected[2] = 0;
        expected[3] = 255;
    } else if (probe_internal_format == GL_RGB8 ||
               probe_internal_format == GL_R11F_G11F_B10F) {
        expected[3] = 255;
    }
}

static int run_case(enum object_kind source_kind, enum object_kind dest_kind,
                    int attachment_index)
{
    const GLenum attachment = GL_COLOR_ATTACHMENT0 + attachment_index;
    GLuint source_fbo = 0, dest_fbo = 0;
    GLuint source = make_color_object(source_kind, 1);
    GLuint dest = dest_kind == OBJECT_DEFAULT ? 0 : make_color_object(dest_kind, 0);
    glGenFramebuffers(1, &source_fbo);
    if (dest_kind != OBJECT_DEFAULT) glGenFramebuffers(1, &dest_fbo);

    glBindFramebuffer(GL_FRAMEBUFFER, source_fbo);
    attach_color(GL_FRAMEBUFFER, attachment, source_kind, source, 1);
    glDrawBuffer(attachment);
    glReadBuffer(attachment);
    GLenum source_status = glCheckFramebufferStatus(GL_FRAMEBUFFER);

    glBindFramebuffer(GL_FRAMEBUFFER, dest_fbo);
    if (dest_kind == OBJECT_DEFAULT) {
        glDrawBuffer(GL_BACK);
        glReadBuffer(GL_BACK);
    } else {
        attach_color(GL_FRAMEBUFFER, attachment, dest_kind, dest, 0);
        glDrawBuffer(attachment);
        glReadBuffer(attachment);
    }
    GLenum dest_status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (source_status != GL_FRAMEBUFFER_COMPLETE ||
        dest_status != GL_FRAMEBUFFER_COMPLETE || glGetError() != GL_NO_ERROR) {
        fprintf(stderr, "FAIL setup src_status=0x%04x dst_status=0x%04x gl=0x%04x\n",
                source_status, dest_status, glGetError());
        return 1;
    }

    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, source_fbo);
    glDrawBuffer(attachment);
    glEnable(GL_SCISSOR_TEST);
    for (int quadrant = 0; quadrant < 4; ++quadrant) {
        GLfloat color[4];
        for (int component = 0; component < 4; ++component)
            color[component] = colors[quadrant][component] / 255.0f;
        glScissor((quadrant & 1) ? 8 : 0, (quadrant & 2) ? 8 : 0, 8, 8);
        glClearColor(color[0], color[1], color[2], color[3]);
        glClear(GL_COLOR_BUFFER_BIT);
    }
    glDisable(GL_SCISSOR_TEST);

    glBindFramebuffer(GL_READ_FRAMEBUFFER, source_fbo);
    glReadBuffer(attachment);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dest_fbo);
    glDrawBuffer(dest_kind == OBJECT_DEFAULT ? GL_BACK : attachment);
    glBlitFramebuffer(0, 0, 16, 16, 0, 0, 16, 16,
                      GL_COLOR_BUFFER_BIT, GL_NEAREST);

    glBindFramebuffer(GL_READ_FRAMEBUFFER, dest_fbo);
    glReadBuffer(dest_kind == OBJECT_DEFAULT ? GL_BACK : attachment);
    int failed = 0;
    for (int quadrant = 0; quadrant < 4; ++quadrant) {
        GLubyte pixel[4] = { 0, 0, 0, 0 };
        glReadPixels((quadrant & 1) ? 12 : 4, (quadrant & 2) ? 12 : 4,
                     1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
        GLubyte expected[4];
        expected_pixel(quadrant, expected);
        printf("PIXEL format=%s src=%s dst=%s attachment=%d quadrant=%d rgba=%u,%u,%u,%u\n",
               probe_format_name,
               source_kind == OBJECT_TEXTURE ? "tex" : "rb",
               dest_kind == OBJECT_TEXTURE ? "tex" :
                   (dest_kind == OBJECT_RENDERBUFFER ? "rb" : "default"),
               attachment_index, quadrant,
               pixel[0], pixel[1], pixel[2], pixel[3]);
        if (!close_pixel(pixel, expected))
            failed = 1;
        if (probe_internal_format == GL_R11F_G11F_B10F ||
            probe_internal_format == GL_RG16F || probe_internal_format == GL_R16F) {
            GLfloat float_pixel[4] = {-1.0f, -1.0f, -1.0f, -1.0f};
            glReadPixels((quadrant & 1) ? 12 : 4, (quadrant & 2) ? 12 : 4,
                         1, 1, GL_RGBA, GL_FLOAT, float_pixel);
            printf("FLOAT format=%s quadrant=%d rgba=%.4f,%.4f,%.4f,%.4f\n",
                   probe_format_name, quadrant, float_pixel[0], float_pixel[1],
                   float_pixel[2], float_pixel[3]);
            for (int component = 0; component < 4; ++component) {
                const float wanted = expected[component] / 255.0f;
                if (float_pixel[component] < wanted - 0.01f ||
                    float_pixel[component] > wanted + 0.01f)
                    failed = 1;
            }
        }
    }
    GLenum error = glGetError();
    printf("CASE format=%s src=%s dst=%s attachment=%d error=0x%04x result=%s\n",
           probe_format_name,
           source_kind == OBJECT_TEXTURE ? "tex" : "rb",
           dest_kind == OBJECT_TEXTURE ? "tex" :
               (dest_kind == OBJECT_RENDERBUFFER ? "rb" : "default"),
           attachment_index, error,
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
        !eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "FAIL egl init error=0x%04x\n", eglGetError());
        return 2;
    }
    EGLConfig config = NULL;
    EGLint count = 0;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE
    };
    eglChooseConfig(display, config_attributes, &config, 1, &count);
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4,
        EGL_CONTEXT_MINOR_VERSION, 4,
        EGL_NONE
    };
    const EGLint surface_attributes[] = {
        EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE
    };
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (count < 1 || context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) {
        fprintf(stderr, "FAIL egl context error=0x%04x\n", eglGetError());
        return 2;
    }

    int failed = 0;
    if (argc >= 4) {
        enum object_kind source = strcmp(argv[1], "rb") == 0
                                      ? OBJECT_RENDERBUFFER : OBJECT_TEXTURE;
        enum object_kind dest = strcmp(argv[2], "rb") == 0 ? OBJECT_RENDERBUFFER :
                                (strcmp(argv[2], "default") == 0 ? OBJECT_DEFAULT :
                                                                  OBJECT_TEXTURE);
        int attachment = atoi(argv[3]);
        int repetitions = argc >= 5 ? atoi(argv[4]) : 1;
        if (argc >= 6) {
            struct format_entry { const char *name; GLenum value; };
            static const struct format_entry formats[] = {
                { "r8", GL_R8 }, { "rg8", GL_RG8 }, { "rgba8", GL_RGBA8 },
                { "srgb8_alpha8", GL_SRGB8_ALPHA8 }, { "rgba4", GL_RGBA4 },
                { "rgb5_a1", GL_RGB5_A1 }, { "r11f_g11f_b10f", GL_R11F_G11F_B10F },
                { "rg16f", GL_RG16F }, { "r16f", GL_R16F }, { "rgb8", GL_RGB8 },
                { "rgb10_a2", GL_RGB10_A2 },
            };
            for (size_t index = 0; index < sizeof(formats) / sizeof(formats[0]); ++index) {
                if (strcmp(argv[5], formats[index].name) == 0) {
                    probe_format_name = formats[index].name;
                    probe_internal_format = formats[index].value;
                    break;
                }
            }
        }
        if (repetitions < 1) repetitions = 1;
        for (int repetition = 0; repetition < repetitions; ++repetition)
            failed += run_case(source, dest, attachment);
        printf("SUMMARY cases=%d failed=%d\n", repetitions, failed);
        return failed ? 1 : 0;
    }
    for (int source = OBJECT_TEXTURE; source <= OBJECT_RENDERBUFFER; ++source)
        for (int dest = OBJECT_TEXTURE; dest <= OBJECT_RENDERBUFFER; ++dest) {
            failed += run_case((enum object_kind)source,
                               (enum object_kind)dest, 0);
            failed += run_case((enum object_kind)source,
                               (enum object_kind)dest, 3);
        }
    printf("SUMMARY cases=8 failed=%d\n", failed);
    return failed ? 1 : 0;
}
