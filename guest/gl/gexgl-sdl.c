/*
 * Make an ordinary SDL2 OpenGL application use gex's direct GL device.
 *
 * SDL continues to create and manage the native X11/Wayland window, so all
 * window-system input and sizing remain completely conventional.  Only the
 * four GL-facing operations are interposed: context creation, proc lookup,
 * making the context current, and swap.  The application and SDL are not
 * patched, and Mesa is not in the command path.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

typedef void *SDL_GLContext;

enum { SDL_WINDOW_OPENGL = 0x00000002u };

int SDL_GL_SetAttribute(int attribute, int value)
{
    static int (*real_set_attribute)(int, int);
    if (!real_set_attribute)
        real_set_attribute = dlsym(RTLD_NEXT, "SDL_GL_SetAttribute");
    if (getenv("GEX_GL_TRACE_CONTEXT"))
        fprintf(stderr, "gex-gl: SDL GL attribute %d = %d\n", attribute, value);
    /* Preserve SDL's attribute bookkeeping even though the direct context is
     * created independently of X11/GLX.  Generic applications can query the
     * requested values back through SDL. */
    return real_set_attribute ? real_set_attribute(attribute, value) : 0;
}

extern void *eglGetDisplay(void *native);
extern unsigned eglInitialize(void *display, int *major, int *minor);
extern unsigned eglBindAPI(unsigned api);
extern void *eglCreateContext(void *display, void *config, void *share,
                              const int *attributes);
extern unsigned eglMakeCurrent(void *display, void *draw, void *read,
                               void *context);
extern unsigned eglSwapBuffers(void *display, void *surface);
extern void *eglGetProcAddress(const char *name);

static void *gex_display;
static void *gex_surface = (void *)2;
static SDL_GLContext gex_context;

void *SDL_CreateWindow(const char *title, int x, int y, int w, int h,
                       uint32_t flags)
{
    static void *(*real_create_window)(const char *, int, int, int, int,
                                       uint32_t);
    if (!real_create_window)
        real_create_window = dlsym(RTLD_NEXT, "SDL_CreateWindow");
    if (!real_create_window)
        return NULL;
    /* The browser canvas is the GL drawable.  Asking Xwayland for a GLX
     * visual would pointlessly require a second renderer just to create the
     * native window, and fails on deliberately Mesa-free root filesystems. */
    return real_create_window(title, x, y, w, h, flags & ~SDL_WINDOW_OPENGL);
}

SDL_GLContext SDL_GL_CreateContext(void *window)
{
    enum { EGL_OPENGL_API = 0x30A2 };
    (void)window;
    if (gex_context)
        return gex_context;
    if (getenv("GEX_GL_TRACE_CONTEXT"))
        fprintf(stderr, "gex-gl: SDL_GL_CreateContext entered\n");
    gex_display = eglGetDisplay(NULL);
    /* SDL_GL_CreateContext is the desktop OpenGL entry point. The direct
     * transport also supports EGL/OpenGL ES clients, so its default API is
     * deliberately ES; bind desktop GL here before creating SDL's context.
     * Without this, unmodified desktop applications see an ES 3.0 identity
     * and select legacy/fallback renderers even though they requested GL. */
    /* Each step named separately: "SDL_GL_CreateContext returned NULL" is not
     * a diagnosis, and this is the exact point where an unmodified desktop
     * application silently loses its GL and falls back to a fixed pipeline. */
    if (!gex_display) {
        fprintf(stderr, "gex-gl: eglGetDisplay failed\n");
        return NULL;
    }
    if (!eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "gex-gl: eglBindAPI(EGL_OPENGL_API) failed\n");
        return NULL;
    }
    if (!eglInitialize(gex_display, NULL, NULL)) {
        fprintf(stderr, "gex-gl: eglInitialize failed\n");
        return NULL;
    }
    gex_context = eglCreateContext(gex_display, (void *)1, NULL, NULL);
    if (!gex_context) {
        fprintf(stderr, "gex-gl: eglCreateContext failed\n");
        return NULL;
    }
    if (!eglMakeCurrent(gex_display, gex_surface, gex_surface, gex_context)) {
        fprintf(stderr, "gex-gl: eglMakeCurrent failed\n");
        return NULL;
    }
    if (getenv("GEX_GL_TRACE_CONTEXT"))
        fprintf(stderr, "gex-gl: SDL_GL_CreateContext succeeded\n");
    return gex_context;
}

void *SDL_GL_GetProcAddress(const char *name)
{
    void *proc = eglGetProcAddress(name);
    if (!proc && getenv("GEX_GL_TRACE_MISSING"))
        fprintf(stderr, "gex-gl: missing %s\n", name ? name : "(null)");
    return proc;
}

int SDL_GL_MakeCurrent(void *window, SDL_GLContext context)
{
    (void)window;
    return eglMakeCurrent(gex_display, gex_surface, gex_surface, context) ? 0 : -1;
}

void SDL_GL_SwapWindow(void *window)
{
    (void)window;
    eglSwapBuffers(gex_display, gex_surface);
}

void SDL_GL_DeleteContext(SDL_GLContext context)
{
    (void)context;
}

int SDL_GL_SetSwapInterval(int interval)
{
    (void)interval;
    return 0;
}

int SDL_GL_GetSwapInterval(void)
{
    return 0;
}

/* Keep the existing guest-runtime workaround independent of STK.  A desktop
 * application may still create its own threads; this only prevents STK's
 * texture loader from sizing a large pool from the browser host CPU count. */
unsigned gex_gl_hardware_concurrency(void)
    __asm__("_ZNSt6thread20hardware_concurrencyEv");

unsigned gex_gl_hardware_concurrency(void)
{
    return 1;
}
