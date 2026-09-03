/* Creating a core-profile context through GLX, which is how every real
 * windowed GL application on X11 does it.
 *
 * The rest of the probes in this directory use EGL with a surfaceless
 * platform, because that is what a headless test wants. Applications do not:
 * GLFW, SDL and Qt all reach for GLX on X11 by default, and they ask for a
 * *specific* version and profile with glXCreateContextAttribsARB. Minecraft
 * 26.2 refuses to start with
 *
 *   BackendCreationException: Driver does not support OpenGL 3.3
 *
 * which comes from glfwCreateWindow failing -- not from a version string --
 * while glxinfo on the same driver reports 4.4 core. So "the driver advertises
 * 4.4" and "an application can obtain a 3.3 core context" are different
 * claims, and this probe is the second one.
 *
 * Each case asks for one (major, minor, profile) combination and reports the
 * version it actually got:
 *
 *   `legacy`       no attributes at all -- glXCreateContext, which is what
 *                  glxgears uses
 *   `core-33`      3.3 core, which is Minecraft's minimum and the one that
 *                  fails
 *   `core-33-fc`   3.3 core plus forward-compatible, which is what GLFW asks
 *                  for when told to
 *   `core-44`      4.4 core, the version the driver claims
 *   `compat-33`    3.3 compatibility, to tell a profile problem from a
 *                  version problem
 */
#include <GL/glx.h>
#include <GL/gl.h>
#include <X11/Xlib.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

typedef GLXContext (*create_attribs_fn)(Display *, GLXFBConfig, GLXContext,
                                        Bool, const int *);

static Display *display;
static GLXFBConfig chosen;
static create_attribs_fn create_attribs;
static Window window;

/* Nothing here reports through a GL error: a refused context is a NULL
 * return, and an X error from the request is delivered asynchronously to the
 * handler below rather than to the caller. */
static int x_error_count;
static char x_error_text[256];

static int handle_x_error(Display *d, XErrorEvent *e)
{
    ++x_error_count;
    if (!x_error_text[0])
        XGetErrorText(d, e->error_code, x_error_text, sizeof(x_error_text) - 1);
    return 0;
}

static void try_context(const char *name, int major, int minor, int profile,
                        int forward_compatible, int want_major,
                        int want_minor)
{
    int attribs[16];
    unsigned n = 0;
    if (major) {
        attribs[n++] = GLX_CONTEXT_MAJOR_VERSION_ARB; attribs[n++] = major;
        attribs[n++] = GLX_CONTEXT_MINOR_VERSION_ARB; attribs[n++] = minor;
    }
    if (profile) {
        attribs[n++] = GLX_CONTEXT_PROFILE_MASK_ARB; attribs[n++] = profile;
    }
    if (forward_compatible) {
        attribs[n++] = GLX_CONTEXT_FLAGS_ARB;
        attribs[n++] = GLX_CONTEXT_FORWARD_COMPATIBLE_BIT_ARB;
    }
    attribs[n] = None;

    x_error_count = 0;
    x_error_text[0] = 0;
    GLXContext context = major || profile || forward_compatible
        ? create_attribs(display, chosen, NULL, True, attribs)
        : glXCreateNewContext(display, chosen, GLX_RGBA_TYPE, NULL, True);
    XSync(display, False);
    int got_major = 0, got_minor = 0;
    const char *version = NULL;
    int made_current = 0;
    if (context) {
        made_current = glXMakeCurrent(display, window, context) ? 1 : 0;
        if (made_current) {
            version = (const char *)glGetString(GL_VERSION);
            glGetIntegerv(GL_MAJOR_VERSION, &got_major);
            glGetIntegerv(GL_MINOR_VERSION, &got_minor);
            while (glGetError() != GL_NO_ERROR) {}
            if (!got_major && version)
                sscanf(version, "%d.%d", &got_major, &got_minor);
        }
        glXMakeCurrent(display, None, NULL);
    }
    fprintf(stderr, "INFO %s context=%p current=%d got=%d.%d version=\"%s\" "
            "xerrors=%d%s%s\n", name, (void *)context, made_current, got_major,
            got_minor, version ? version : "(none)", x_error_count,
            x_error_text[0] ? " " : "", x_error_text);
    const int ok = context && made_current &&
                   (got_major > want_major ||
                    (got_major == want_major && got_minor >= want_minor));
    result(name, ok);
    if (context)
        glXDestroyContext(display, context);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    XSetErrorHandler(handle_x_error);
    display = XOpenDisplay(NULL);
    if (!display) {
        fprintf(stderr, "INFO no X display\n");
        return 2;
    }
    int major = 0, minor = 0;
    if (!glXQueryVersion(display, &major, &minor)) return 2;
    fprintf(stderr, "INFO glx=%d.%d vendor=\"%s\"\n", major, minor,
            glXGetClientString(display, GLX_VENDOR));

    static const int config_attribs[] = {
        GLX_X_RENDERABLE, True,
        GLX_DRAWABLE_TYPE, GLX_WINDOW_BIT,
        GLX_RENDER_TYPE, GLX_RGBA_BIT,
        GLX_RED_SIZE, 8, GLX_GREEN_SIZE, 8, GLX_BLUE_SIZE, 8,
        GLX_ALPHA_SIZE, 8, GLX_DEPTH_SIZE, 24, GLX_DOUBLEBUFFER, True,
        None,
    };
    int count = 0;
    GLXFBConfig *configs = glXChooseFBConfig(display, DefaultScreen(display),
                                             config_attribs, &count);
    if (!configs || count < 1) {
        fprintf(stderr, "INFO no fbconfig\n");
        return 2;
    }
    chosen = configs[0];
    XVisualInfo *visual = glXGetVisualFromFBConfig(display, chosen);
    if (!visual) return 2;
    XSetWindowAttributes swa;
    memset(&swa, 0, sizeof(swa));
    swa.colormap = XCreateColormap(display, RootWindow(display, visual->screen),
                                   visual->visual, AllocNone);
    window = XCreateWindow(display, RootWindow(display, visual->screen), 0, 0,
                           64, 64, 0, visual->depth, InputOutput,
                           visual->visual, CWColormap, &swa);
    create_attribs = (create_attribs_fn)glXGetProcAddressARB(
        (const GLubyte *)"glXCreateContextAttribsARB");
    fprintf(stderr, "INFO create_attribs=%p\n", (void *)create_attribs);
    if (!create_attribs) {
        result("legacy", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    try_context("legacy", 0, 0, 0, 0, 1, 0);
    try_context("core-33", 3, 3, GLX_CONTEXT_CORE_PROFILE_BIT_ARB, 0, 3, 3);
    try_context("core-33-fc", 3, 3, GLX_CONTEXT_CORE_PROFILE_BIT_ARB, 1, 3, 3);
    try_context("core-44", 4, 4, GLX_CONTEXT_CORE_PROFILE_BIT_ARB, 0, 4, 4);
    try_context("compat-33", 3, 3,
                GLX_CONTEXT_COMPATIBILITY_PROFILE_BIT_ARB, 0, 3, 3);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
