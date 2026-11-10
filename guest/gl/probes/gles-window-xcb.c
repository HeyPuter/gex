/* gles-window as an XCB client: libxcb alone, no Xlib anywhere in the
 * process, and EGL through EGL_EXT_platform_xcb, as wgpu reaches it under
 * x11rb (gpui, for one). libxcb is reached through dlopen, so this builds
 * with EGL's and GLES's headers and nothing else.
 *
 *   gles-window-xcb [frames] [seconds]   (default: 60 frames, then 30 s shown)
 *
 * Says "window 0x<xid>", then "surface WxH visual same|other" (the surface's
 * size and whether EGL_NATIVE_VISUAL_ID is the screen's root visual), then
 * "frames N gl error E" once they are swapped, then keeps the window up,
 * swapping a frame a second, until the seconds are over. */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define WIDTH 200
#define HEIGHT 150

#ifndef EGL_PLATFORM_XCB_EXT
#define EGL_PLATFORM_XCB_EXT 0x31DC
#define EGL_PLATFORM_XCB_SCREEN_EXT 0x31DE
#endif

/* libxcb's own layouts (the X protocol's), the fields used here. */
struct cookie { unsigned sequence; };
struct screen {
    uint32_t root, default_colormap, white_pixel, black_pixel, input_masks;
    uint16_t width, height, mm_width, mm_height, min_maps, max_maps;
    uint32_t root_visual;
};
struct screen_iterator { struct screen *data; int rem, index; };

int main(int argc, char **argv) {
    const int frames = argc > 1 ? atoi(argv[1]) : 60;
    const int seconds = argc > 2 ? atoi(argv[2]) : 30;
    setvbuf(stdout, NULL, _IONBF, 0);
    void *x = dlopen("libxcb.so.1", RTLD_NOW | RTLD_GLOBAL);
    if (!x) { printf("no libxcb: %s\n", dlerror()); return 1; }
    if (dlopen("libX11.so.6", RTLD_LAZY | RTLD_NOLOAD)) { printf("Xlib is loaded\n"); return 1; }
    void *(*connect)(const char *, int *) = dlsym(x, "xcb_connect");
    int (*has_error)(void *) = dlsym(x, "xcb_connection_has_error");
    const void *(*get_setup)(void *) = dlsym(x, "xcb_get_setup");
    struct screen_iterator (*roots)(const void *) = dlsym(x, "xcb_setup_roots_iterator");
    void (*next)(struct screen_iterator *) = dlsym(x, "xcb_screen_next");
    uint32_t (*generate_id)(void *) = dlsym(x, "xcb_generate_id");
    struct cookie (*create)(void *, uint8_t, uint32_t, uint32_t, int16_t, int16_t, uint16_t, uint16_t, uint16_t, uint16_t, uint32_t, uint32_t, const void *) = dlsym(x, "xcb_create_window");
    struct cookie (*change_property)(void *, uint8_t, uint32_t, uint32_t, uint32_t, uint8_t, uint32_t, const void *) = dlsym(x, "xcb_change_property");
    struct cookie (*map)(void *, uint32_t) = dlsym(x, "xcb_map_window");
    int (*flush)(void *) = dlsym(x, "xcb_flush");
    if (!connect || !has_error || !get_setup || !roots || !next || !generate_id || !create || !change_property || !map || !flush) { printf("libxcb is missing calls\n"); return 1; }
    int screen_number = 0;
    void *c = connect(NULL, &screen_number);
    if (!c || has_error(c)) { printf("no X connection\n"); return 1; }
    struct screen_iterator it = roots(get_setup(c));
    for (int i = 0; i < screen_number && it.rem; i++) next(&it);
    if (!it.rem || !it.data) { printf("no screen %d\n", screen_number); return 1; }
    const struct screen *scr = it.data;
    const uint32_t win = generate_id(c);
    create(c, 0 /* CopyFromParent */, win, scr->root, 0, 0, WIDTH, HEIGHT, 0, 1 /* InputOutput */, scr->root_visual, 0, NULL);
    const char *name = "gles-window-xcb";
    change_property(c, 0 /* Replace */, win, 39 /* WM_NAME */, 31 /* STRING */, 8, (uint32_t)strlen(name), name);
    map(c, win);
    flush(c);
    printf("window 0x%x\n", win);

    const char *client = eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS);
    if (!client || !strstr(client, "EGL_EXT_platform_xcb")) { printf("no EGL_EXT_platform_xcb: %s\n", client ? client : "(none)"); return 1; }
    const EGLAttrib dpy_attrs[] = { EGL_PLATFORM_XCB_SCREEN_EXT, screen_number, EGL_NONE };
    EGLDisplay egl = eglGetPlatformDisplay(EGL_PLATFORM_XCB_EXT, c, dpy_attrs);
    if (egl == EGL_NO_DISPLAY || !eglInitialize(egl, NULL, NULL)) { printf("eglInitialize failed\n"); return 1; }
    const EGLint cfg_attrs[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                                 EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(egl, cfg_attrs, &cfg, 1, &n) || n < 1) { printf("no EGL config\n"); return 1; }
    EGLint visual = -1;
    eglGetConfigAttrib(egl, cfg, EGL_NATIVE_VISUAL_ID, &visual);
    eglBindAPI(EGL_OPENGL_ES_API);
    /* The platform call takes a pointer to the window id. */
    uint32_t native = win;
    EGLSurface surf = eglCreatePlatformWindowSurface(egl, cfg, &native, NULL);
    if (surf == EGL_NO_SURFACE) { printf("no window surface\n"); return 1; }
    EGLint sw = 0, sh = 0;
    eglQuerySurface(egl, surf, EGL_WIDTH, &sw);
    eglQuerySurface(egl, surf, EGL_HEIGHT, &sh);
    printf("surface %dx%d visual %s\n", sw, sh, (uint32_t)visual == scr->root_visual ? "same" : "other");
    const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(egl, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(egl, surf, surf, ctx)) { printf("no context\n"); return 1; }
    for (int i = 0; i < frames; i++) {
        glViewport(0, 0, WIDTH, HEIGHT);
        glClearColor(1.0f, 0.0f, 1.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        eglSwapBuffers(egl, surf);
    }
    printf("frames %d gl error %u\n", frames, glGetError());
    for (int s = 0; s < seconds; s++) {
        sleep(1);
        glClear(GL_COLOR_BUFFER_BIT);
        eglSwapBuffers(egl, surf);
    }
    printf("done\n");
    return 0;
}
