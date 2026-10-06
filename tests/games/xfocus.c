/* xfocus: give the largest mapped top-level X window the input focus, every
 * two seconds -- a window manager's one job that games need. The in-guest
 * Xwayland runs none, so a window never gets FocusIn, and SDL (Unity 6) then
 * believes the game is in the background (Application.isFocused false).
 *
 *   DISPLAY=:N xfocus     (runs until the X server goes away) */
#include <X11/Xlib.h>
#include <stdio.h>
#include <unistd.h>

static int ignore (Display *d, XErrorEvent *e) { (void)d; (void)e; return 0; }

int main (void)
{
	Display *d = XOpenDisplay (NULL);
	if (!d) {
		fprintf (stderr, "xfocus: no display\n");
		return 1;
	}
	XSetErrorHandler (ignore);
	fprintf (stderr, "xfocus: watching %s\n", DisplayString (d));
	Window last = 0;
	for (;;) {
		Window root = DefaultRootWindow (d), r, p, *kids = NULL, best = 0;
		unsigned n = 0;
		long best_area = 0;
		if (XQueryTree (d, root, &r, &p, &kids, &n)) {
			for (unsigned i = 0; i < n; i++) {
				XWindowAttributes a;
				if (!XGetWindowAttributes (d, kids [i], &a) || a.map_state != IsViewable || a.class != InputOutput)
					continue;
				long area = (long)a.width * a.height;
				if (area > best_area) {
					best_area = area;
					best = kids [i];
				}
			}
			if (kids)
				XFree (kids);
		}
		if (best) {
			Window f;
			int rev;
			XGetInputFocus (d, &f, &rev);
			if (f != best) {
				XRaiseWindow (d, best);
				XSetInputFocus (d, best, RevertToParent, CurrentTime);
				if (best != last)
					fprintf (stderr, "xfocus: focus to window %#lx\n", (unsigned long)best);
				last = best;
			}
			XFlush (d);
		}
		if (XPending (d) < 0)
			break;
		sleep (2);
	}
	return 0;
}
