#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

static int equal4f(const GLfloat *v, GLfloat a, GLfloat b, GLfloat c, GLfloat d) {
    return v[0] == a && v[1] == b && v[2] == c && v[3] == d;
}

static int equal4d(const GLdouble *v, GLdouble a, GLdouble b, GLdouble c, GLdouble d) {
    return v[0] == a && v[1] == b && v[2] == c && v[3] == d;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attribs[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE
    };
    EGLConfig config = NULL;
    EGLint count = 0;
    if (!eglChooseConfig(display, config_attribs, &config, 1, &count) || count < 1) return 3;
    const EGLint surface_attribs[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attribs);
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    if (surface == EGL_NO_SURFACE || context == EGL_NO_CONTEXT ||
        !eglMakeCurrent(display, surface, surface, context)) return 4;

    glViewport(3, 5, 640, 360);
    glColorMask(GL_TRUE, GL_FALSE, GL_TRUE, GL_FALSE);
    glPixelStoref(GL_PACK_ALIGNMENT, 3.6f);

    GLfloat viewport_f[6] = {-91.0f, 0, 0, 0, 0, 73.0f};
    GLdouble viewport_d[6] = {-19.0, 0, 0, 0, 0, 37.0};
    GLboolean mask[6] = {71, 0, 0, 0, 0, 93};
    GLint alignment = 0;
    glGetFloatv(GL_VIEWPORT, viewport_f + 1);
    glGetDoublev(GL_VIEWPORT, viewport_d + 1);
    glGetBooleanv(GL_COLOR_WRITEMASK, mask + 1);
    glGetIntegerv(GL_PACK_ALIGNMENT, &alignment);

    int fixed = viewport_f[0] == -91.0f && viewport_f[5] == 73.0f &&
                equal4f(viewport_f + 1, 3, 5, 640, 360) &&
                viewport_d[0] == -19.0 && viewport_d[5] == 37.0 &&
                equal4d(viewport_d + 1, 3, 5, 640, 360) &&
                mask[0] == 71 && mask[5] == 93 &&
                mask[1] == GL_TRUE && mask[2] == GL_FALSE &&
                mask[3] == GL_TRUE && mask[4] == GL_FALSE && alignment == 4;

    GLint formats = 0;
    glGetIntegerv(GL_NUM_COMPRESSED_TEXTURE_FORMATS, &formats);
    int dynamic = formats >= 0 && formats <= 4096;
    GLfloat *format_f = calloc((size_t)formats + 2, sizeof(*format_f));
    GLdouble *format_d = calloc((size_t)formats + 2, sizeof(*format_d));
    GLboolean *format_b = calloc((size_t)formats + 2, sizeof(*format_b));
    if (!format_f || !format_d || !format_b) return 5;
    format_f[0] = -123.0f; format_f[formats + 1] = 321.0f;
    format_d[0] = -456.0; format_d[formats + 1] = 654.0;
    format_b[0] = 17; format_b[formats + 1] = 29;
    glGetFloatv(GL_COMPRESSED_TEXTURE_FORMATS, format_f + 1);
    glGetDoublev(GL_COMPRESSED_TEXTURE_FORMATS, format_d + 1);
    glGetBooleanv(GL_COMPRESSED_TEXTURE_FORMATS, format_b + 1);
    dynamic = dynamic && format_f[0] == -123.0f && format_f[formats + 1] == 321.0f &&
              format_d[0] == -456.0 && format_d[formats + 1] == 654.0 &&
              format_b[0] == 17 && format_b[formats + 1] == 29;
    for (GLint i = 0; i < formats; ++i) {
        dynamic = dynamic && format_d[i + 1] == (GLdouble)format_f[i + 1] &&
                  format_b[i + 1] == (format_f[i + 1] != 0 ? GL_TRUE : GL_FALSE);
    }

    GLenum error = glGetError();
    printf("STATE fixed=%d dynamic=%d formats=%d error=0x%x\n", fixed, dynamic, formats, error);
    const int clean = error == GL_NO_ERROR;
    const int passed_count = fixed + dynamic + clean;
    const int passed = passed_count == 3;
    printf("SUMMARY cases=3 passed=%d failed=%d\n", passed_count, 3 - passed_count);
    free(format_f); free(format_d); free(format_b);
    return passed ? 0 : 1;
}
