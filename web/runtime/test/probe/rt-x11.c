/* An X11 window for the runtime's window tests, through the page's X server:
 * one window of a solid colour, titled from argv, that says on stdout what
 * it hears: "mapped", "key KEYCODE" for each key pressed, and "close" when
 * the window manager asks it to close (WM_DELETE_WINDOW), which ends it
 * with 0.
 *
 *   rt-x11 TITLE */
#include <stdio.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>

int main(int argc, char **argv) {
  const char *title = argc > 1 ? argv[1] : "rt-x11";
  Display *dpy = XOpenDisplay(NULL);
  if (!dpy) {
    fprintf(stderr, "rt-x11: no X display\n");
    return 1;
  }
  int scr = DefaultScreen(dpy);
  Window w = XCreateSimpleWindow(dpy, RootWindow(dpy, scr), 0, 0, 160, 80, 0, 0, 0x3060c0);
  XStoreName(dpy, w, title);
  Atom del = XInternAtom(dpy, "WM_DELETE_WINDOW", False);
  XSetWMProtocols(dpy, w, &del, 1);
  XSelectInput(dpy, w, ExposureMask | KeyPressMask | StructureNotifyMask);
  XMapWindow(dpy, w);
  for (;;) {
    XEvent e;
    XNextEvent(dpy, &e);
    if (e.type == MapNotify) {
      printf("mapped\n");
    } else if (e.type == KeyPress) {
      printf("key %u\n", e.xkey.keycode);
    } else if (e.type == ClientMessage && (Atom)e.xclient.data.l[0] == del) {
      printf("close\n");
      fflush(stdout);
      break;
    }
    fflush(stdout);
  }
  XCloseDisplay(dpy);
  return 0;
}
