/* gles-window-xcb at an offset from the screen's corner, with a plain window
 * stacked over part of it: on a rootful X screen, the GL frame must show in
 * the window's place and the window above must hide what it covers. Both are
 * override-redirect, so no window manager moves them.
 *
 *   gles-window-xcb-under [seconds] [width] [height]
 *     (default: 30 s, 200x150)
 *
 * The GL window is at (40, 30), cleared magenta; the cover is solid green
 * (pixel 0x00ff00), 100x80 at the GL window's centre. Says "window 0x<xid>
 * cover 0x<xid>", then "surface WxH", then swaps as fast as it is let for
 * the seconds, saying "swaps N in S s" each second and "done" at the end. */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define X 40
#define Y 30
#define COVER_W 100
#define COVER_H 80

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

#define CW_BACK_PIXEL 0x2
#define CW_OVERRIDE_REDIRECT 0x200

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    const int seconds = argc > 1 ? atoi(argv[1]) : 30;
    const int width = argc > 2 ? atoi(argv[2]) : 200;
    const int height = argc > 3 ? atoi(argv[3]) : 150;
    setvbuf(stdout, NULL, _IONBF, 0);
    void *x = dlopen("libxcb.so.1", RTLD_NOW | RTLD_GLOBAL);
    if (!x) { printf("no libxcb: %s\n", dlerror()); return 1; }
    void *(*connect)(const char *, int *) = dlsym(x, "xcb_connect");
    int (*has_error)(void *) = dlsym(x, "xcb_connection_has_error");
    const void *(*get_setup)(void *) = dlsym(x, "xcb_get_setup");
    struct screen_iterator (*roots)(const void *) = dlsym(x, "xcb_setup_roots_iterator");
    void (*next)(struct screen_iterator *) = dlsym(x, "xcb_screen_next");
    uint32_t (*generate_id)(void *) = dlsym(x, "xcb_generate_id");
    struct cookie (*create)(void *, uint8_t, uint32_t, uint32_t, int16_t, int16_t, uint16_t, uint16_t, uint16_t, uint16_t, uint32_t, uint32_t, const void *) = dlsym(x, "xcb_create_window");
    struct cookie (*map)(void *, uint32_t) = dlsym(x, "xcb_map_window");
    int (*flush)(void *) = dlsym(x, "xcb_flush");
    if (!connect || !has_error || !get_setup || !roots || !next || !generate_id || !create || !map || !flush) { printf("libxcb is missing calls\n"); return 1; }
    int screen_number = 0;
    void *c = connect(NULL, &screen_number);
    if (!c || has_error(c)) { printf("no X connection\n"); return 1; }
    struct screen_iterator it = roots(get_setup(c));
    for (int i = 0; i < screen_number && it.rem; i++) next(&it);
    if (!it.rem || !it.data) { printf("no screen %d\n", screen_number); return 1; }
    const struct screen *scr = it.data;
    const uint32_t win = generate_id(c);
    const uint32_t over[] = { 1 };
    create(c, 0 /* CopyFromParent */, win, scr->root, X, Y, width, height, 0, 1 /* InputOutput */, scr->root_visual, CW_OVERRIDE_REDIRECT, over);
    map(c, win);
    /* Mapped after it, so above it. */
    const uint32_t cover = generate_id(c);
    const uint32_t green[] = { 0x00ff00, 1 };
    create(c, 0, cover, scr->root, X + width / 2, Y + height / 2, COVER_W, COVER_H, 0, 1, scr->root_visual, CW_BACK_PIXEL | CW_OVERRIDE_REDIRECT, green);
    map(c, cover);
    flush(c);
    printf("window 0x%x cover 0x%x\n", win, cover);

    const EGLAttrib dpy_attrs[] = { EGL_PLATFORM_XCB_SCREEN_EXT, screen_number, EGL_NONE };
    EGLDisplay egl = eglGetPlatformDisplay(EGL_PLATFORM_XCB_EXT, c, dpy_attrs);
    if (egl == EGL_NO_DISPLAY || !eglInitialize(egl, NULL, NULL)) { printf("eglInitialize failed\n"); return 1; }
    const EGLint cfg_attrs[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                                 EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(egl, cfg_attrs, &cfg, 1, &n) || n < 1) { printf("no EGL config\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    uint32_t native = win;
    EGLSurface surf = eglCreatePlatformWindowSurface(egl, cfg, &native, NULL);
    if (surf == EGL_NO_SURFACE) { printf("no window surface\n"); return 1; }
    EGLint sw = 0, sh = 0;
    eglQuerySurface(egl, surf, EGL_WIDTH, &sw);
    eglQuerySurface(egl, surf, EGL_HEIGHT, &sh);
    printf("surface %dx%d\n", sw, sh);
    const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(egl, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(egl, surf, surf, ctx)) { printf("no context\n"); return 1; }
    const double end = now() + seconds;
    double mark = now();
    int swaps = 0;
    while (now() < end) {
        glViewport(0, 0, width, height);
        glClearColor(1.0f, 0.0f, 1.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        eglSwapBuffers(egl, surf);
        swaps++;
        const double t = now();
        if (t - mark >= 1.0) {
            printf("swaps %d in %.2f s\n", swaps, t - mark);
            swaps = 0;
            mark = t;
        }
    }
    printf("done\n");
    return 0;
}
