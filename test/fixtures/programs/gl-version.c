/* Prints the desktop GL version the guest's EGL gives a context that asks for
 * none: the most this page's adapter supports, which decides which of the GL
 * probes can run here at all. */
#include <EGL/egl.h>
#include <stdio.h>

/* GL's own header is not in every guest's toolchain; this is all of it used. */
#define GL_VERSION 0x1F02
extern const unsigned char *glGetString(unsigned int name);

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(d, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return 1;
    const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                         EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE};
    EGLConfig c = NULL;
    EGLint n = 0;
    if (!eglChooseConfig(d, ca, &c, 1, &n) || !n) return 2;
    const EGLint sa[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (x == EGL_NO_CONTEXT || s == EGL_NO_SURFACE || !eglMakeCurrent(d, s, s, x)) return 3;
    const char *v = (const char *)glGetString(GL_VERSION);
    if (!v) return 4;
    printf("%s\n", v);
    return 0;
}
