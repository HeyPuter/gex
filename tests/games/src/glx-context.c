/* GLX contexts as SDL2 makes them: a throwaway legacy context, then the one
 * wanted, from glXCreateContextAttribsARB. The second must be what it asked
 * for (gex's guest GL used to make every context the host's compatibility
 * one at glXMakeCurrent). Run by tests/games/glx-context.sh. */
#include <X11/Xlib.h>
#include <GL/glx.h>
#include <stdio.h>
#include <string.h>

typedef GLXContext (*cca_t)(Display *, GLXFBConfig, GLXContext, Bool, const int *);
static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

int main(void) {
    Display *d = XOpenDisplay(NULL);
    if (!d) { printf("FAIL no display\n"); return 1; }
    int att[] = {GLX_RGBA, GLX_DOUBLEBUFFER, None};
    XVisualInfo *vi = glXChooseVisual(d, DefaultScreen(d), att);
    XSetWindowAttributes a = {0};
    a.colormap = XCreateColormap(d, RootWindow(d, vi->screen), vi->visual, AllocNone);
    Window w = XCreateWindow(d, RootWindow(d, vi->screen), 0, 0, 64, 64, 0, vi->depth, InputOutput, vi->visual, CWColormap, &a);
    XMapWindow(d, w);
    XFlush(d);
    int n = 0;
    GLXFBConfig *cfg = glXChooseFBConfig(d, DefaultScreen(d), NULL, &n);
    cca_t cca = (cca_t)glXGetProcAddressARB((const GLubyte *)"glXCreateContextAttribsARB");
    CHECK("glXCreateContextAttribsARB", cca && n > 0);
    if (!cca || !n) return 1;

    struct { const char *name; int major, minor, mask; } want[] = {
        {"3.2 core", 3, 2, 1}, {"2.1 compatibility", 2, 1, 2}, {"ES 2.0", 2, 0, 4},
    };
    for (unsigned i = 0; i < sizeof want / sizeof want[0]; i++) {
        /* SDL's throwaway context first. */
        GLXContext t = glXCreateContext(d, vi, NULL, True);
        glXMakeCurrent(d, w, t);
        glXMakeCurrent(d, None, NULL);
        glXDestroyContext(d, t);
        int ca[] = {0x2091, want[i].major, 0x2092, want[i].minor, 0x9126, want[i].mask, 0};
        GLXContext c = cca(d, cfg[0], NULL, True, ca);
        int made = c && glXMakeCurrent(d, w, c);
        const char *v = (const char *)glGetString(GL_VERSION);
        GLint mask = 0;
        glGetIntegerv(0x9126 /* GL_CONTEXT_PROFILE_MASK */, &mask);
        printf("  %s: %s, profile mask %d\n", want[i].name, v ? v : "(null)", mask);
        char what[64];
        snprintf(what, sizeof what, "%s: made current", want[i].name);
        CHECK(what, made && glXGetCurrentContext() == c);
        snprintf(what, sizeof what, "%s: what it asked for", want[i].name);
        if (want[i].mask == 4) CHECK(what, v && !strncmp(v, "OpenGL ES", 9));
        else if (want[i].mask == 1) CHECK(what, v && v[0] >= '3' && (mask & 1));
        else CHECK(what, v && v[0] >= '2' && strncmp(v, "OpenGL ES", 9) && !(mask & 1));
        if (want[i].mask == 2) {
            /* What Baba Is You requires by name, and the one command that
               used to keep GL_ARB_shader_objects from being claimed. */
            const char *e = (const char *)glGetString(GL_EXTENSIONS);
            CHECK("compatibility: GL_ARB_shader_objects claimed", e && strstr(e, "GL_ARB_shader_objects"));
            typedef unsigned (*create_t)(unsigned);
            typedef void (*del_t)(unsigned);
            typedef unsigned char (*is_t)(unsigned);
            create_t cs = (create_t)glXGetProcAddressARB((const GLubyte *)"glCreateShaderObjectARB");
            del_t del = (del_t)glXGetProcAddressARB((const GLubyte *)"glDeleteObjectARB");
            is_t is_shader = (is_t)glXGetProcAddressARB((const GLubyte *)"glIsShader");
            unsigned sh = cs ? cs(0x8B31) : 0;
            int was = sh && is_shader && is_shader(sh);
            if (del && sh) del(sh);
            CHECK("glDeleteObjectARB deletes a shader", was && !is_shader(sh));
        }
        glXMakeCurrent(d, None, NULL);
        glXDestroyContext(d, c);
    }
    printf("%d failures\n", failures);
    return failures;
}
