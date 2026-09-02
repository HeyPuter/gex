/* Minimal libepoxy runtime queries for virglrenderer hosted by MobileGL.
 * The GL entry points themselves are exported by MobileGL; unlike the WebGL
 * renderer, this host really does present a desktop OpenGL 3.3 interface. */
#include <stdbool.h>
#include <string.h>
#define GL_GLEXT_PROTOTYPES 1
#include <GL/gl.h>
#include <GL/glext.h>

int epoxy_gl_version(void) { return 33; }
bool epoxy_is_desktop_gl(void) { return true; }
bool epoxy_has_gl_extension(const char *ext) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const char *candidate = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (candidate && strcmp(candidate, ext) == 0) return true;
    }
    return false;
}
bool epoxy_has_glx_extension(void *display, int screen, const char *ext) {
    (void)display;
    (void)screen;
    (void)ext;
    return false;
}

int pthread_setname_np(void *thread, const char *name) {
    (void)thread;
    (void)name;
    return 0;
}
