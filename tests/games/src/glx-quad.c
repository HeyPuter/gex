/* An X11 GLX window, legacy context, fixed function: clear red, a green quad,
 * swap, forever; after 120 frames the GL window is reparented into an
 * override-redirect window, as SDL 1.2 does for fullscreen. What a page with
 * hostx=1 should keep showing (tests/games/browser-linux.mjs --exe glx-quad). */
#include <X11/Xlib.h>
#include <GL/glx.h>
#include <stdio.h>
#include <unistd.h>
int main(void) {
    Display *d = XOpenDisplay(NULL);
    if (!d) { printf("no display\n"); return 1; }
    int att[] = {GLX_RGBA, GLX_DOUBLEBUFFER, GLX_DEPTH_SIZE, 16, None};
    XVisualInfo *vi = glXChooseVisual(d, DefaultScreen(d), att);
    XSetWindowAttributes a = {0};
    a.colormap = XCreateColormap(d, RootWindow(d, vi->screen), vi->visual, AllocNone);
    Window top = XCreateWindow(d, RootWindow(d, vi->screen), 0, 0, 320, 240, 0, vi->depth, InputOutput, vi->visual, CWColormap, &a);
    /* Like SDL 1.2: the GL drawable is a child of the managed window. */
    Window w = XCreateWindow(d, top, 0, 0, 320, 240, 0, vi->depth, InputOutput, vi->visual, CWColormap, &a);
    XMapWindow(d, w); XMapWindow(d, top); XFlush(d);
    GLXContext c = glXCreateContext(d, vi, NULL, True);
    glXMakeCurrent(d, w, c);
    printf("GL %s\n", glGetString(GL_VERSION)); fflush(stdout);
    for (int i = 0;; i++) {
        glViewport(0, 0, 320, 240);
        glClearColor(1, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glColor3f(0, 1, 0);
        glBegin(GL_QUADS);
        glVertex2f(-0.5f, -0.5f); glVertex2f(0.5f, -0.5f); glVertex2f(0.5f, 0.5f); glVertex2f(-0.5f, 0.5f);
        glEnd();
        glXSwapBuffers(d, w);
        if (i % 60 == 0) { printf("frame %d\n", i); fflush(stdout); }
        if (i == 120) {
            /* SDL 1.2's fullscreen: an override-redirect window over the
               screen, the GL window reparented into it. */
            XSetWindowAttributes o = {0};
            o.override_redirect = True;
            o.colormap = a.colormap;
            Window fs = XCreateWindow(d, RootWindow(d, vi->screen), 0, 0, 320, 240, 0, vi->depth, InputOutput, vi->visual, CWOverrideRedirect | CWColormap, &o);
            XMapRaised(d, fs);
            XReparentWindow(d, w, fs, 0, 0);
            XFlush(d);
            printf("reparented into 0x%lx\n", fs); fflush(stdout);
        }
        usleep(16000);
    }
}
