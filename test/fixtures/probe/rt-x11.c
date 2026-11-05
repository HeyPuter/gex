/* An X11 window for the runtime's window tests, through the page's X server:
 * one window of a solid colour, titled from argv, that says on stdout what
 * it hears: "mapped", "key KEYCODE" for each key pressed, "button N" for
 * each button pressed, and "close" when
 * the window manager asks it to close (WM_DELETE_WINDOW), which ends it
 * with 0. Its WM_CLASS is "rt-x11", "RtX11" and its _NET_WM_PID its pid,
 * as a toolkit's window's are.
 *
 *   rt-x11 TITLE [copy TEXT] [paste] [look] [where] [at]
 *
 * With "copy TEXT" it owns the clipboard with TEXT once mapped ("copied",
 * and "sent" each time it is asked); with "paste", each button press asks
 * for the clipboard as UTF8_STRING and says "paste TEXT"; with "look", a
 * press grabs the pointer, hides it (XFixes, which is what Xwayland takes
 * for a hidden cursor: a blank cursor image is still a cursor) and warps it
 * to the window's middle, as a game's mouse look does, and each motion after says "look DX DY" (the
 * distance from the middle, warped back each time). With "where", each
 * ConfigureNotify of its window says "at X Y W H": where the window is on
 * the root (as the page placed it) and its size; and the root's, when the
 * output changes size, "root W H" (once at the start too). With "at", each
 * press also says "press N at X Y" and each motion "motion X Y", in the
 * window's own coordinates. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/Xlib-xcb.h>
#include <xcb/xfixes.h>

int main(int argc, char **argv) {
  const char *title = argc > 1 ? argv[1] : "rt-x11";
  const char *copy = NULL;
  int paste = 0, look = 0, looking = 0, where = 0, at = 0;
  for (int i = 2; i < argc; i++) {
    if (!strcmp(argv[i], "copy") && i + 1 < argc) copy = argv[++i];
    else if (!strcmp(argv[i], "paste")) paste = 1;
    else if (!strcmp(argv[i], "look")) look = 1;
    else if (!strcmp(argv[i], "where")) where = 1;
    else if (!strcmp(argv[i], "at")) at = 1;
  }
  Display *dpy = XOpenDisplay(NULL);
  if (!dpy) {
    fprintf(stderr, "rt-x11: no X display\n");
    return 1;
  }
  int scr = DefaultScreen(dpy);
  Window w = XCreateSimpleWindow(dpy, RootWindow(dpy, scr), 0, 0, 160, 80, 0, 0, 0x3060c0);
  XStoreName(dpy, w, title);
  XClassHint class = {"rt-x11", "RtX11"};
  XSetClassHint(dpy, w, &class);
  long pid = getpid();
  XChangeProperty(dpy, w, XInternAtom(dpy, "_NET_WM_PID", False), XA_CARDINAL, 32, PropModeReplace, (unsigned char *)&pid, 1);
  Atom del = XInternAtom(dpy, "WM_DELETE_WINDOW", False);
  Atom clipboard = XInternAtom(dpy, "CLIPBOARD", False);
  Atom utf8 = XInternAtom(dpy, "UTF8_STRING", False);
  Atom targets = XInternAtom(dpy, "TARGETS", False);
  Atom prop = XInternAtom(dpy, "RT_PASTE", False);
  XSetWMProtocols(dpy, w, &del, 1);
  XSelectInput(dpy, w, ExposureMask | KeyPressMask | StructureNotifyMask | ButtonPressMask | PointerMotionMask);
  if (where) {
    XSelectInput(dpy, RootWindow(dpy, scr), StructureNotifyMask);
    printf("root %d %d\n", DisplayWidth(dpy, scr), DisplayHeight(dpy, scr));
    fflush(stdout);
  }
  XMapWindow(dpy, w);
  for (;;) {
    XEvent e;
    XNextEvent(dpy, &e);
    if (e.type == MapNotify) {
      printf("mapped\n");
      if (copy) {
        XSetSelectionOwner(dpy, clipboard, w, CurrentTime);
        printf("%s\n", XGetSelectionOwner(dpy, clipboard) == w ? "copied" : "not copied");
      }
    } else if (e.type == ConfigureNotify && where && e.xconfigure.window == RootWindow(dpy, scr)) {
      printf("root %d %d\n", e.xconfigure.width, e.xconfigure.height);
    } else if (e.type == ConfigureNotify && where) {
      int x, y;
      Window child;
      XTranslateCoordinates(dpy, w, RootWindow(dpy, scr), 0, 0, &x, &y, &child);
      printf("at %d %d %d %d\n", x, y, e.xconfigure.width, e.xconfigure.height);
    } else if (e.type == KeyPress) {
      printf("key %u\n", e.xkey.keycode);
    } else if (e.type == ButtonPress) {
      printf("button %u\n", e.xbutton.button);
      if (at) printf("press %u at %d %d\n", e.xbutton.button, e.xbutton.x, e.xbutton.y);
      if (paste) XConvertSelection(dpy, clipboard, utf8, prop, w, e.xbutton.time);
      if (look && !looking) {
        looking = 1;
        static char none[8];
        XColor black = {0};
        Pixmap empty = XCreateBitmapFromData(dpy, w, none, 8, 8);
        Cursor hidden = XCreatePixmapCursor(dpy, empty, empty, &black, &black, 0, 0);
        XDefineCursor(dpy, w, hidden);
        xcb_connection_t *c = XGetXCBConnection(dpy);
        free(xcb_xfixes_query_version_reply(c, xcb_xfixes_query_version(c, 5, 0), NULL));
        xcb_xfixes_hide_cursor(c, w);
        xcb_flush(c);
        int g = XGrabPointer(dpy, w, True, PointerMotionMask | ButtonPressMask, GrabModeAsync, GrabModeAsync, w, hidden,
                             CurrentTime);
        XWarpPointer(dpy, None, w, 0, 0, 0, 0, 80, 40);
        printf("grabbed %d\n", g);
      }
    } else if (e.type == MotionNotify && at && !looking) {
      printf("motion %d %d\n", e.xmotion.x, e.xmotion.y);
    } else if (e.type == MotionNotify && looking) {
      int dx = e.xmotion.x - 80, dy = e.xmotion.y - 40;
      if (dx || dy) {
        printf("look %d %d\n", dx, dy);
        XWarpPointer(dpy, None, w, 0, 0, 0, 0, 80, 40);
      }
    } else if (e.type == SelectionRequest) {
      XSelectionRequestEvent *r = &e.xselectionrequest;
      XSelectionEvent n = {SelectionNotify, 0, True, dpy, r->requestor, r->selection, r->target, None, r->time};
      if (copy && r->target == targets) {
        Atom t[2] = {targets, utf8};
        XChangeProperty(dpy, r->requestor, r->property, XA_ATOM, 32, PropModeReplace, (unsigned char *)t, 2);
        n.property = r->property;
      } else if (copy && (r->target == utf8 || r->target == XA_STRING)) {
        XChangeProperty(dpy, r->requestor, r->property, r->target, 8, PropModeReplace, (const unsigned char *)copy,
                        (int)strlen(copy));
        n.property = r->property;
      }
      XSendEvent(dpy, r->requestor, False, 0, (XEvent *)&n);
      printf("sent\n");
    } else if (e.type == SelectionNotify && e.xselection.property == prop) {
      Atom type;
      int format;
      unsigned long items, after;
      unsigned char *data = NULL;
      XGetWindowProperty(dpy, w, prop, 0, 65536, True, AnyPropertyType, &type, &format, &items, &after, &data);
      printf("paste %s\n", data ? (char *)data : "");
      if (data) XFree(data);
    } else if (e.type == SelectionNotify) {
      printf("paste refused\n");
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
