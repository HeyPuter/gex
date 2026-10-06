/* An X window that prints the input it gets, one line per event, unbuffered:
 * the check that scripted input (GEX_WL_INPUT) reaches an X client through the
 * in-guest Xwayland.
 *   xinput-echo [SECONDS]   exits after SECONDS (default 60) */
#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

int
main (int argc, char **argv)
{
	int secs = argc > 1 ? atoi (argv [1]) : 60;
	setvbuf (stdout, NULL, _IONBF, 0);
	printf ("xinput-echo: opening the display\n");
	Display *d = XOpenDisplay (NULL);
	if (!d) { printf ("no display\n"); return 1; }
	Window w = XCreateSimpleWindow (d, DefaultRootWindow (d), 0, 0, 800, 600, 0, 0, 0x336699);
	XSelectInput (d, w, KeyPressMask | KeyReleaseMask | ButtonPressMask | ButtonReleaseMask | PointerMotionMask | EnterWindowMask | ExposureMask | FocusChangeMask);
	XMapWindow (d, w);
	XFlush (d);
	printf ("xinput-echo: window mapped\n");
	time_t end = time (NULL) + secs;
	setvbuf (stdout, NULL, _IONBF, 0);
	int motions = 0;
	time_t beat = time (NULL);
	while (time (NULL) < end) {
		if (time (NULL) >= beat + 5) { beat = time (NULL); printf ("alive, %d motions\n", motions); }
		while (XPending (d)) {
			XEvent e;
			XNextEvent (d, &e);
			switch (e.type) {
			case MotionNotify: if (motions++ % 20 == 0) printf ("motion %d %d\n", e.xmotion.x, e.xmotion.y); break;
			case ButtonPress: printf ("button press %u at %d %d\n", e.xbutton.button, e.xbutton.x, e.xbutton.y); break;
			case ButtonRelease: printf ("button release %u\n", e.xbutton.button); break;
			case KeyPress: printf ("key press %u\n", e.xkey.keycode); break;
			case KeyRelease: printf ("key release %u\n", e.xkey.keycode); break;
			case EnterNotify: printf ("enter %d %d\n", e.xcrossing.x, e.xcrossing.y); break;
			case FocusIn: printf ("focus in\n"); break;
			case Expose: if (e.xexpose.count == 0) { XClearWindow (d, w); XFlush (d); } break;
			}
		}
		struct timespec ts = { 0, 20000000 };
		nanosleep (&ts, NULL);
	}
	printf ("motions %d\n", motions);
	return 0;
}
