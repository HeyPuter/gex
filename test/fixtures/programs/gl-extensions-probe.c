/* Whether the GL context lists an extension, by glGetStringi and by
 * glGetString(GL_EXTENSIONS): one line per name asked for,
 *   EXT <name> stringi=<0|1> string=<0|1|none>
 * then `COUNT <n>`. Run natively for the host's answer and under
 * `gexc run --gl` for the guest library's (test/gl/withheld).
 *
 *   cc -o probe gl-extensions-probe.c -lEGL -lGL
 *   probe GL_AMD_pinned_memory ...
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

/* `name` as a whole word of the space-separated `list`. */
static int listed(const char *list, const char *name) {
    size_t n = strlen(name);
    for (const char *p = list; (p = strstr(p, name)); p += n) {
        if ((p == list || p[-1] == ' ') && (p[n] == ' ' || p[n] == 0)) return 1;
    }
    return 0;
}

int main(int argc, char **argv) {
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "FATAL egl_initialize error=0x%x\n", eglGetError());
        return 2;
    }
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count) || config_count < 1) return 2;
    const EGLint context_attributes[] = {EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 4, EGL_NONE};
    const EGLint surface_attributes[] = {EGL_WIDTH, 16, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE || !eglMakeCurrent(display, surface, surface, context))
        return 2;

    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    /* A core context may answer GL_EXTENSIONS with an error and no string. */
    const char *all = (const char *)glGetString(GL_EXTENSIONS);
    for (int i = 1; i < argc; i++) {
        int by_index = 0;
        for (GLint e = 0; e < count; e++) {
            const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)e);
            if (name && !strcmp(name, argv[i])) by_index = 1;
        }
        printf("EXT %s stringi=%d string=%s\n", argv[i], by_index,
               all ? (listed(all, argv[i]) ? "1" : "0") : "none");
    }
    printf("COUNT %d\n", count);
    return 0;
}
