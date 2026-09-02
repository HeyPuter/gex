/*
 * A desktop-GL context through EGL, and what it says about itself.
 *
 * The GLX side of this question has `glx-init`; this is the other one. A
 * client that reaches the device through EGL rather than GLX -- which is every
 * Wayland client, and SDL when it takes its EGL backend -- selects desktop GL
 * with `eglBindAPI` and a version with context attributes, and then has to be
 * told the truth about what it got: applications choose a renderer by parsing
 * `GL_VERSION`, and an empty or invented string sends them down the wrong
 * path with no way to find out.
 *
 * No window: a pbuffer needs neither a compositor nor an X server, so this
 * asks only about the context. Everything is reached through dlopen, so it
 * builds with a compiler and nothing else.
 *
 *   egl-init [frames]        (default: 60)
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>

enum {
    EGL_OPENGL_API = 0x30A2, EGL_OPENGL_ES_API = 0x30A0,
    EGL_NONE = 0x3038, EGL_WIDTH = 0x3057, EGL_HEIGHT = 0x3056,
    EGL_CONTEXT_MAJOR_VERSION = 0x3098, EGL_CONTEXT_MINOR_VERSION = 0x30FB,
    EGL_SURFACE_TYPE = 0x3033, EGL_PBUFFER_BIT = 0x0001,
    EGL_RENDERABLE_TYPE = 0x3040, EGL_OPENGL_BIT = 0x0008,
    EGL_RED_SIZE = 0x3024, EGL_GREEN_SIZE = 0x3023, EGL_BLUE_SIZE = 0x3022,
    EGL_ALPHA_SIZE = 0x3021, EGL_DEPTH_SIZE = 0x3025,
    EGL_VERSION = 0x3054, EGL_VENDOR = 0x3053,
    GL_VENDOR = 0x1F00, GL_RENDERER = 0x1F01, GL_VERSION_STR = 0x1F02,
    GL_COLOR_BUFFER_BIT = 0x4000,
};

static int failures;

static void *need(void *lib, const char *name)
{
    void *sym = dlsym(lib, name);
    if (!sym) {
        printf("egl-init: %s is missing\n", name);
        failures++;
    }
    return sym;
}

int main(int argc, char **argv)
{
    const int frames = argc > 1 ? atoi(argv[1]) : 60;
    void *egl = dlopen("libEGL.so.1", RTLD_LAZY);
    if (!egl) {
        printf("egl-init: no libEGL.so.1\n");
        return 1;
    }

    void *(*get_display)(void *) = need(egl, "eglGetDisplay");
    unsigned (*initialize)(void *, int *, int *) = need(egl, "eglInitialize");
    unsigned (*bind_api)(unsigned) = need(egl, "eglBindAPI");
    unsigned (*choose_config)(void *, const int *, void **, int, int *) =
        need(egl, "eglChooseConfig");
    void *(*create_context)(void *, void *, void *, const int *) =
        need(egl, "eglCreateContext");
    void *(*create_pbuffer)(void *, void *, const int *) =
        need(egl, "eglCreatePbufferSurface");
    unsigned (*make_current)(void *, void *, void *, void *) = need(egl, "eglMakeCurrent");
    unsigned (*swap_buffers)(void *, void *) = need(egl, "eglSwapBuffers");
    unsigned (*destroy_context)(void *, void *) = need(egl, "eglDestroyContext");
    void *(*get_proc)(const char *) = need(egl, "eglGetProcAddress");
    const char *(*query_string)(void *, int) = need(egl, "eglQueryString");
    if (failures) return 1;

    void *dpy = get_display(NULL);
    int major = 0, minor = 0;
    if (!dpy || !initialize(dpy, &major, &minor)) {
        printf("egl-init: eglInitialize failed\n");
        return 1;
    }
    printf("egl-init: EGL %d.%d, %s\n", major, minor,
           query_string(dpy, EGL_VERSION) ? query_string(dpy, EGL_VERSION) : "?");

    /* Desktop GL, said the way EGL says it. Without this the device's default
       is OpenGL ES, and an application that wanted desktop GL is handed an ES
       context whose version string then sends it down its legacy path. */
    if (!bind_api(EGL_OPENGL_API)) {
        printf("egl-init: eglBindAPI(EGL_OPENGL_API) refused\n");
        return 1;
    }

    const int config_attribs[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
        EGL_ALPHA_SIZE, 8, EGL_DEPTH_SIZE, 24, EGL_NONE,
    };
    void *config = NULL;
    int configs = 0;
    if (!choose_config(dpy, config_attribs, &config, 1, &configs) || configs < 1) {
        printf("egl-init: no config for a desktop-GL pbuffer\n");
        return 1;
    }

    const int context_attribs[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 2, EGL_NONE,
    };
    void *ctx = create_context(dpy, config, NULL, context_attribs);
    const int surface_attribs[] = { EGL_WIDTH, 256, EGL_HEIGHT, 256, EGL_NONE };
    void *surface = create_pbuffer(dpy, config, surface_attribs);
    if (!ctx || !surface) {
        printf("egl-init: no %s\n", ctx ? "pbuffer" : "context");
        return 1;
    }
    if (!make_current(dpy, surface, surface, ctx)) {
        printf("egl-init: eglMakeCurrent failed\n");
        return 1;
    }

    const char *(*get_string)(unsigned) = get_proc("glGetString");
    void (*clear_color)(float, float, float, float) = get_proc("glClearColor");
    void (*clear)(unsigned) = get_proc("glClear");
    if (!get_string || !clear_color || !clear) {
        printf("egl-init: the GL entry points are unreachable\n");
        return 1;
    }
    const char *vendor = get_string(GL_VENDOR);
    const char *renderer = get_string(GL_RENDERER);
    const char *version = get_string(GL_VERSION_STR);
    printf("egl-init: vendor %s\n", vendor && *vendor ? vendor : "(empty)");
    printf("egl-init: renderer %s\n", renderer && *renderer ? renderer : "(empty)");
    printf("egl-init: version %s\n", version && *version ? version : "(empty)");
    /* An application cannot choose a renderer without these, so an empty one
       is a failure and not a curiosity. */
    if (!version || !*version) {
        printf("egl-init: GL_VERSION is empty; nothing can decide what to draw with\n");
        failures++;
    }

    for (int i = 0; i < frames; i++) {
        const float t = (float)i / (frames > 1 ? (float)(frames - 1) : 1.f);
        clear_color(0.2f, t, 1.f - t, 1.f);
        clear(GL_COLOR_BUFFER_BIT);
        swap_buffers(dpy, surface);
    }
    printf("egl-init: %d frames swapped\n", frames);

    make_current(dpy, NULL, NULL, NULL);
    destroy_context(dpy, ctx);
    printf("egl-init: %s\n", failures ? "FAILED" : "ok");
    return failures ? 1 : 0;
}
