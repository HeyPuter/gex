/* xfocus: give the top-level X window under the pointer (else the largest) the input focus, every
 * two seconds -- a window manager's one job that games need. The in-guest
 * Xwayland runs none, so a window never gets FocusIn, and SDL (Unity 6) then
 * believes the game is in the background (Application.isFocused false).
 *
 *   DISPLAY=:N xfocus     (runs until the X server goes away) */
#include <X11/Xlib.h>
#include <stdio.h>
#include <dlfcn.h>
#include <stdlib.h>
#include <unistd.h>

static int ignore (Display *d, XErrorEvent *e) { (void)d; (void)e; return 0; }

/* XInput2's selection query, declared here (no XInput2 headers needed; libXi is loaded
 * at run time, and missing it only loses the XI2 column). */
typedef struct { int deviceid; int mask_len; unsigned char *mask; } XIMask;
static XIMask *(*xi_selected) (Display *, Window, int *);

/* XFOCUS_TREE=1: the window tree (three levels) whenever the top-level set or the focus
 * changes, with each window's core event masks and its XI2 selections (device: mask bits). */
static void
tree (Display *d, Window w, int depth)
{
	Window r, p, *kids = NULL;
	unsigned n = 0;
	XWindowAttributes a;
	char *name = NULL;
	if (!XGetWindowAttributes (d, w, &a))
		return;
	XFetchName (d, w, &name);
	fprintf (stderr, "xfocus: %*s%#lx %dx%d+%d+%d %s%s masks %#lx '%s'\n", depth * 2, "", (unsigned long)w,
		a.width, a.height, a.x, a.y, a.map_state == IsViewable ? "viewable" : a.map_state == IsUnmapped ? "unmapped" : "unviewable",
		a.class == InputOnly ? " input-only" : a.override_redirect ? " override" : "", a.all_event_masks, name ? name : "");
	if (name)
		XFree (name);
	int nm = 0;
	XIMask *m = xi_selected ? xi_selected (d, w, &nm) : NULL;
	for (int i = 0; m && i < nm; i++) {
		unsigned long bits = 0;
		for (int b = 0; b < m [i].mask_len && b < 8; b++)
			bits |= (unsigned long)m [i].mask [b] << (8 * b);
		fprintf (stderr, "xfocus: %*s  xi2 device %d mask %#lx\n", depth * 2, "", m [i].deviceid, bits);
	}
	if (m)
		XFree (m);
	if (depth >= 3 || !XQueryTree (d, w, &r, &p, &kids, &n))
		return;
	for (unsigned i = 0; i < n; i++)
		tree (d, kids [i], depth + 1);
	if (kids)
		XFree (kids);
}

int main (void)
{
	Display *d = XOpenDisplay (NULL);
	if (!d) {
		fprintf (stderr, "xfocus: no display\n");
		return 1;
	}
	XSetErrorHandler (ignore);
	void *xi = dlopen ("libXi.so.6", RTLD_NOW);
	if (xi)
		xi_selected = (XIMask *(*) (Display *, Window, int *))dlsym (xi, "XIGetSelectedEvents");
	fprintf (stderr, "xfocus: watching %s\n", DisplayString (d));
	Window last = 0;
	/* The top-level windows are listed whenever their number changes. */
	int verbose = 0;
	unsigned last_count = 0;
	for (;;) {
		Window root = DefaultRootWindow (d), r, p, *kids = NULL, best = 0;
		unsigned n = 0;
		long best_area = 0;
		if (XQueryTree (d, root, &r, &p, &kids, &n)) {
			verbose = n != last_count;
			last_count = n;
			if (verbose && getenv ("XFOCUS_TREE")) {
				Window f;
				int rev;
				XGetInputFocus (d, &f, &rev);
				fprintf (stderr, "xfocus: tree (focus %#lx)\n", (unsigned long)f);
				tree (d, root, 0);
			}
			for (unsigned i = 0; i < n; i++) {
				XWindowAttributes a;
				if (!XGetWindowAttributes (d, kids [i], &a) || a.map_state != IsViewable || a.class != InputOutput)
					continue;
				long area = (long)a.width * a.height;
				if (verbose) {
					char *name = NULL;
					XFetchName (d, kids [i], &name);
					fprintf (stderr, "xfocus: candidate %#lx %dx%d+%d+%d masks %#lx '%s'\n", (unsigned long)kids [i],
						a.width, a.height, a.x, a.y, a.all_event_masks, name ? name : "");
					if (name)
						XFree (name);
				}
				if (area > best_area) {
					best_area = area;
					best = kids [i];
				}
			}
			if (kids)
				XFree (kids);
		}
		/* The window under the pointer, if it is a viewable top-level, wins: that is the
		 * window the game's pointer input already reaches (Unity 6 maps two 800x600
		 * top-levels, and keys sent to the other one were lost). */
		Window pr, pc;
		int rx, ry, wx, wy;
		unsigned mask;
		if (XQueryPointer (d, root, &pr, &pc, &rx, &ry, &wx, &wy, &mask) && pc) {
			XWindowAttributes a;
			if (XGetWindowAttributes (d, pc, &a) && a.map_state == IsViewable && a.class == InputOutput)
				best = pc;
		}
		if (best) {
			Window f;
			int rev;
			XGetInputFocus (d, &f, &rev);
			if (f != best) {
				XSetInputFocus (d, best, RevertToParent, CurrentTime);
				if (best != last) {
					fprintf (stderr, "xfocus: focus to window %#lx (was %#lx)\n", (unsigned long)best, (unsigned long)f);
					if (getenv ("XFOCUS_TREE"))
						tree (d, root, 0);
				}
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
