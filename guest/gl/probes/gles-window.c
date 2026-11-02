/* A GLES2 window on X11 through EGL: what a game's window is, at its
 * smallest. An X window, an EGL window surface on it, and frames cleared to
 * one colour and swapped, so a test can look at the page and see the guest's
 * GL there. Xlib is reached through dlopen, as SDL reaches it, so this builds
 * with EGL's and GLES's headers and nothing else.
 *
 *   gles-window [frames] [seconds]   (default: 60 frames, then 30 s shown)
 *
 * Says "window 0x<xid>", "frames N" once they are swapped, then keeps the
 * window up, swapping a frame a second, until the seconds are over. */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define WIDTH 200
#define HEIGHT 150

int main(int argc, char **argv) {
    const int frames = argc > 1 ? atoi(argv[1]) : 60;
    const int seconds = argc > 2 ? atoi(argv[2]) : 30;
    setvbuf(stdout, NULL, _IONBF, 0);
    void *x = dlopen("libX11.so.6", RTLD_NOW | RTLD_GLOBAL);
    if (!x) { printf("no libX11: %s\n", dlerror()); return 1; }
    void *(*open_display)(const char *) = dlsym(x, "XOpenDisplay");
    int (*default_screen)(void *) = dlsym(x, "XDefaultScreen");
    unsigned long (*root_window)(void *, int) = dlsym(x, "XRootWindow");
    unsigned long (*create)(void *, unsigned long, int, int, unsigned, unsigned, unsigned, unsigned long, unsigned long) = dlsym(x, "XCreateSimpleWindow");
    int (*store_name)(void *, unsigned long, const char *) = dlsym(x, "XStoreName");
    int (*map)(void *, unsigned long) = dlsym(x, "XMapWindow");
    int (*sync)(void *, int) = dlsym(x, "XSync");
    if (!open_display || !default_screen || !root_window || !create || !store_name || !map || !sync) { printf("Xlib is missing calls\n"); return 1; }
    void *dpy = open_display(NULL);
    if (!dpy) { printf("no X display\n"); return 1; }
    const int screen = default_screen(dpy);
    const unsigned long win = create(dpy, root_window(dpy, screen), 0, 0, WIDTH, HEIGHT, 0, 0, 0);
    store_name(dpy, win, "gles-window");
    map(dpy, win);
    sync(dpy, 0);
    printf("window 0x%lx\n", win);

    EGLDisplay egl = eglGetDisplay((EGLNativeDisplayType)dpy);
    if (!eglInitialize(egl, NULL, NULL)) { printf("eglInitialize failed\n"); return 1; }
    const EGLint cfg_attrs[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                                 EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(egl, cfg_attrs, &cfg, 1, &n) || n < 1) { printf("no EGL config\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLSurface surf = eglCreateWindowSurface(egl, cfg, (EGLNativeWindowType)win, NULL);
    if (surf == EGL_NO_SURFACE) { printf("no window surface\n"); return 1; }
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
