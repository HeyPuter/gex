/*
 * An SDL-shaped GLX initialization, without SDL.
 *
 * SDL's X11 backend does something no small demo does, and it is what a GLX
 * implementation has to survive: it creates a window and a context purely to
 * read the GL strings, destroys both, and only then creates the ones the
 * application will use -- with different attributes. Toolkits generally do
 * some version of this, and SuperTuxKart (the SDL application in these
 * images) needed a preloaded interposer for years because this library could
 * not take it.
 *
 * So this program is that sequence, step by step, each step reporting what it
 * got. Everything is reached through dlopen, exactly as SDL reaches libGL:
 * there are no build-time dependencies beyond a C compiler, and nothing here
 * needs X11 or GL headers.
 *
 *   glx-init [frames]        (default: 60)
 *
 * A line per step, and "glx-init: ok" at the end only if every one of them
 * did what GLX says it should.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Xlib and GLX, declared rather than included -- the layouts here are stable
 * ABI that every X client already depends on. */
struct visual_info {
    void *visual;
    unsigned long visualid;
    int screen;
    int depth;
    int class_;
    unsigned long red_mask, green_mask, blue_mask;
    int colormap_size;
    int bits_per_rgb;
};

struct set_window_attributes {
    unsigned long background_pixmap, background_pixel;
    unsigned long border_pixmap, border_pixel;
    int bit_gravity, win_gravity, backing_store;
    unsigned long backing_planes, backing_pixel;
    int save_under;
    long event_mask;
    long do_not_propagate_mask;
    int override_redirect;
    unsigned long colormap, cursor;
};

enum {
    CWBackPixel = 1L << 1, CWBorderPixel = 1L << 3, CWColormap = 1L << 13,
    AllocNone = 0, InputOutput = 1, StructureNotifyMask = 1L << 17,
};

/* GLX attribute names, from glx.h and GLX_ARB_create_context. */
enum {
    GLX_USE_GL = 1, GLX_RGBA = 4, GLX_DOUBLEBUFFER = 5,
    GLX_RED_SIZE = 8, GLX_GREEN_SIZE = 9, GLX_BLUE_SIZE = 10,
    GLX_ALPHA_SIZE = 11, GLX_DEPTH_SIZE = 12, GLX_STENCIL_SIZE = 13,
    GLX_DRAWABLE_TYPE = 0x8010, GLX_RENDER_TYPE = 0x8011,
    GLX_X_RENDERABLE = 0x8012, GLX_WINDOW_BIT = 1, GLX_RGBA_BIT = 1,
    GLX_CONTEXT_MAJOR_VERSION_ARB = 0x2091,
    GLX_CONTEXT_MINOR_VERSION_ARB = 0x2092,
    GLX_CONTEXT_PROFILE_MASK_ARB = 0x9126,
    GLX_CONTEXT_CORE_PROFILE_BIT_ARB = 0x1,
    GLX_CONTEXT_COMPATIBILITY_PROFILE_BIT_ARB = 0x2,
};

static int failures;

static void *need(void *lib, const char *name)
{
    void *sym = dlsym(lib, name);
    if (!sym) {
        printf("glx-init: %s is missing\n", name);
        failures++;
    }
    return sym;
}

int main(int argc, char **argv)
{
    const int frames = argc > 1 ? atoi(argv[1]) : 60;

    void *x11 = dlopen("libX11.so.6", RTLD_LAZY);
    void *gl = dlopen("libGL.so.1", RTLD_LAZY);
    if (!x11 || !gl) {
        printf("glx-init: no %s\n", x11 ? "libGL.so.1" : "libX11.so.6");
        return 1;
    }

    void *(*open_display)(const char *) = need(x11, "XOpenDisplay");
    int (*default_screen)(void *) = need(x11, "XDefaultScreen");
    unsigned long (*root_window)(void *, int) = need(x11, "XRootWindow");
    unsigned long (*create_colormap)(void *, unsigned long, void *, int) =
        need(x11, "XCreateColormap");
    unsigned long (*create_window)(void *, unsigned long, int, int, unsigned,
                                   unsigned, unsigned, int, unsigned, void *,
                                   unsigned long, struct set_window_attributes *) =
        need(x11, "XCreateWindow");
    int (*map_window)(void *, unsigned long) = need(x11, "XMapWindow");
    int (*destroy_window)(void *, unsigned long) = need(x11, "XDestroyWindow");
    int (*x_sync)(void *, int) = need(x11, "XSync");
    int (*store_name)(void *, unsigned long, const char *) = need(x11, "XStoreName");

    int (*query_extension)(void *, int *, int *) = need(gl, "glXQueryExtension");
    int (*query_version)(void *, int *, int *) = need(gl, "glXQueryVersion");
    const char *(*extensions_string)(void *, int) = need(gl, "glXQueryExtensionsString");
    struct visual_info *(*choose_visual)(void *, int, const int *) =
        need(gl, "glXChooseVisual");
    void **(*choose_fbconfig)(void *, int, const int *, int *) =
        need(gl, "glXChooseFBConfig");
    struct visual_info *(*visual_from_fbconfig)(void *, void *) =
        need(gl, "glXGetVisualFromFBConfig");
    void *(*create_context)(void *, void *, void *, int) = need(gl, "glXCreateContext");
    void (*destroy_context)(void *, void *) = need(gl, "glXDestroyContext");
    int (*make_current)(void *, unsigned long, void *) = need(gl, "glXMakeCurrent");
    void (*swap_buffers)(void *, unsigned long) = need(gl, "glXSwapBuffers");
    void *(*get_proc_address)(const unsigned char *) = need(gl, "glXGetProcAddress");
    if (failures) return 1;

    void *dpy = open_display(NULL);
    if (!dpy) {
        printf("glx-init: no display\n");
        return 1;
    }
    const int screen = default_screen(dpy);
    const unsigned long root = root_window(dpy, screen);

    int error_base = 0, event_base = 0;
    printf("glx-init: glXQueryExtension %s\n",
           query_extension(dpy, &error_base, &event_base) ? "yes" : "NO");
    if (!query_extension(dpy, &error_base, &event_base)) failures++;
    int major = 0, minor = 0;
    query_version(dpy, &major, &minor);
    printf("glx-init: GLX %d.%d\n", major, minor);
    if (major < 1 || (major == 1 && minor < 3)) {
        printf("glx-init: GLX 1.3 is the least a toolkit will use\n");
        failures++;
    }

    /* Step one, as a toolkit does it: a visual, a window and a context that
       exist only to be asked what this driver is. */
    const int visual_attribs[] = {
        GLX_RGBA, GLX_DOUBLEBUFFER, GLX_RED_SIZE, 8, GLX_GREEN_SIZE, 8,
        GLX_BLUE_SIZE, 8, GLX_DEPTH_SIZE, 16, 0,
    };
    struct visual_info *probe_visual = choose_visual(dpy, screen, visual_attribs);
    if (!probe_visual) {
        printf("glx-init: glXChooseVisual found nothing\n");
        return 1;
    }
    printf("glx-init: probe visual id 0x%lx depth %d\n",
           probe_visual->visualid, probe_visual->depth);

    struct set_window_attributes attrs;
    memset(&attrs, 0, sizeof attrs);
    attrs.colormap = create_colormap(dpy, root, probe_visual->visual, AllocNone);
    attrs.event_mask = StructureNotifyMask;
    unsigned long probe_window =
        create_window(dpy, root, 0, 0, 16, 16, 0, probe_visual->depth,
                      InputOutput, probe_visual->visual,
                      CWBorderPixel | CWColormap, &attrs);
    void *probe_context = create_context(dpy, probe_visual, NULL, 1);
    if (!probe_window || !probe_context) {
        printf("glx-init: no probe window or context\n");
        return 1;
    }
    if (!make_current(dpy, probe_window, probe_context)) {
        printf("glx-init: the probe context would not go current\n");
        return 1;
    }

    const char *(*get_string)(unsigned) = get_proc_address((const unsigned char *)"glGetString");
    if (!get_string) {
        printf("glx-init: glXGetProcAddress does not know glGetString\n");
        return 1;
    }
    printf("glx-init: probe reports %s / %s\n",
           get_string(0x1F02 /* GL_VERSION */) ? get_string(0x1F02) : "?",
           get_string(0x1F00 /* GL_VENDOR */) ? get_string(0x1F00) : "?");
    const char *exts = extensions_string(dpy, screen);
    printf("glx-init: extensions %s\n", exts ? exts : "(none)");
    if (!exts || !strstr(exts, "GLX_ARB_create_context")) {
        printf("glx-init: without GLX_ARB_create_context a toolkit stops here\n");
        failures++;
    }

    /* And throws all of it away. */
    make_current(dpy, 0, NULL);
    destroy_context(dpy, probe_context);
    destroy_window(dpy, probe_window);
    x_sync(dpy, 0);
    printf("glx-init: probe window and context destroyed\n");

    /* Step two: the real window, through the fbconfig path, with an explicit
       version and profile -- which is the request that used to be ignored
       because the host context had already been made for the probe. */
    const int fb_attribs[] = {
        GLX_X_RENDERABLE, 1, GLX_DRAWABLE_TYPE, GLX_WINDOW_BIT,
        GLX_RENDER_TYPE, GLX_RGBA_BIT, GLX_RED_SIZE, 8, GLX_GREEN_SIZE, 8,
        GLX_BLUE_SIZE, 8, GLX_ALPHA_SIZE, 8, GLX_DEPTH_SIZE, 24,
        GLX_STENCIL_SIZE, 8, GLX_DOUBLEBUFFER, 1, 0,
    };
    int config_count = 0;
    void **configs = choose_fbconfig(dpy, screen, fb_attribs, &config_count);
    if (!configs || config_count < 1) {
        printf("glx-init: glXChooseFBConfig found nothing\n");
        return 1;
    }
    printf("glx-init: %d fbconfig(s)\n", config_count);
    struct visual_info *visual = visual_from_fbconfig(dpy, configs[0]);
    if (!visual) {
        printf("glx-init: no visual for that fbconfig\n");
        return 1;
    }

    memset(&attrs, 0, sizeof attrs);
    attrs.colormap = create_colormap(dpy, root, visual->visual, AllocNone);
    attrs.event_mask = StructureNotifyMask;
    unsigned long window =
        create_window(dpy, root, 0, 0, 300, 300, 0, visual->depth,
                      InputOutput, visual->visual,
                      CWBorderPixel | CWColormap, &attrs);
    if (!window) {
        printf("glx-init: no window\n");
        return 1;
    }
    store_name(dpy, window, "glx-init");
    map_window(dpy, window);

    void *(*create_context_attribs)(void *, void *, void *, int, const int *) =
        get_proc_address((const unsigned char *)"glXCreateContextAttribsARB");
    if (!create_context_attribs) {
        printf("glx-init: glXCreateContextAttribsARB is unreachable\n");
        return 1;
    }
    const int context_attribs[] = {
        GLX_CONTEXT_MAJOR_VERSION_ARB, 3,
        GLX_CONTEXT_MINOR_VERSION_ARB, 2,
        GLX_CONTEXT_PROFILE_MASK_ARB, GLX_CONTEXT_COMPATIBILITY_PROFILE_BIT_ARB,
        0,
    };
    void *context = create_context_attribs(dpy, configs[0], NULL, 1, context_attribs);
    if (!context) {
        printf("glx-init: glXCreateContextAttribsARB gave nothing\n");
        return 1;
    }
    if (!make_current(dpy, window, context)) {
        printf("glx-init: the real context would not go current\n");
        return 1;
    }
    printf("glx-init: real context reports %s\n",
           get_string(0x1F02) ? get_string(0x1F02) : "?");

    void (*clear_color)(float, float, float, float) =
        get_proc_address((const unsigned char *)"glClearColor");
    void (*clear)(unsigned) = get_proc_address((const unsigned char *)"glClear");
    void (*viewport)(int, int, int, int) =
        get_proc_address((const unsigned char *)"glViewport");
    if (!clear_color || !clear || !viewport) {
        printf("glx-init: the GL entry points are unreachable\n");
        return 1;
    }
    viewport(0, 0, 300, 300);

    /* Frames, with a redundant make-current in every one of them: engines do
       that, and each used to cost a host surface until there were none left. */
    for (int i = 0; i < frames; i++) {
        if (!make_current(dpy, window, context)) {
            printf("glx-init: make-current failed on frame %d\n", i);
            failures++;
            break;
        }
        const float t = (float)i / (frames > 1 ? (float)(frames - 1) : 1.f);
        clear_color(t, 1.f - t, 0.25f, 1.f);
        clear(0x00004000 /* GL_COLOR_BUFFER_BIT */);
        swap_buffers(dpy, window);
    }
    printf("glx-init: %d frames swapped\n", frames);

    printf("glx-init: %s\n", failures ? "FAILED" : "ok");
    return failures ? 1 : 0;
}
