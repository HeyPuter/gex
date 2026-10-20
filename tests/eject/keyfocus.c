/* keyfocus: which Win32 window a key reaches when no window was activated
 * (tests/eject/keyfocus.sh). Prints one line per event, "kf: ...", flushed.
 *   A    320x240 at 100,100, activated while still hidden, then shown
 *        without activation (SW_SHOWNA): active in Win32, but never said so
 *        to the compositor
 *   OFF  160x40 at -32000,-32000, shown without activation after A, so it is
 *        on top of the stack but off the output (where Win32 parks a
 *        minimized window)
 * The page's compositor has no window with the keyboard. The first key must reach A (the topmost toplevel on the output),
 * never OFF. Then B (320x240 at 500,100) is shown and activated
 * (SetForegroundWindow), and A is raised over it without activation: B has
 * the keyboard and A is on top, and the next key must reach B. Then A and B
 * are destroyed and N (320x200 at 100,400) is shown without activation:
 * nothing is active in Win32 and no window has the keyboard, and the next key
 * must reach N (winewayland activates a placed window that is given the
 * keyboard while nothing is active).
 *   keyfocus [SECONDS]   exits after SECONDS (default 300) */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static HWND a, off, b, n;

static void say(const char *f, ...)
{
	va_list ap;
	va_start(ap, f);
	printf("kf: ");
	vprintf(f, ap);
	printf("\n");
	fflush(stdout);
	va_end(ap);
}

static const char *name(HWND w)
{
	return !w ? "none" : w == a ? "A" : w == off ? "OFF" : w == b ? "B" : w == n ? "N" : "?";
}

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp)
{
	switch (m) {
	case WM_KEYDOWN:
		say("%s key 0x%02x", name(w), (unsigned)wp);
		if (w == a && !b) {
			b = CreateWindowExA(0, "kf", "kf-b", WS_OVERLAPPEDWINDOW, 500, 100, 320, 240, NULL, NULL, NULL, NULL);
			ShowWindow(b, SW_SHOW);
			SetForegroundWindow(b);
			SetWindowPos(a, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
			say("B up, foreground %s", name(GetForegroundWindow()));
		} else if (w == b && wp == 'C' && !n) {
			/* All three go, so nothing is active (Wine would activate
			 * OFF, the next in z-order, if it stayed); N comes up
			 * without activation: inactive in Win32, and no window has
			 * the keyboard. */
			DestroyWindow(off);
			DestroyWindow(b);
			DestroyWindow(a);
			a = b = off = (HWND)1;
			n = CreateWindowExA(0, "kf", "kf-n", WS_OVERLAPPEDWINDOW, 100, 400, 320, 200, NULL, NULL, NULL, NULL);
			ShowWindow(n, SW_SHOWNOACTIVATE);
			UpdateWindow(n);
			say("N up, foreground %s", name(GetForegroundWindow()));
		}
		return 0;
	case WM_ACTIVATE:
		if (LOWORD(wp) != WA_INACTIVE) say("%s activated", name(w));
		break;
	case WM_PAINT: {
		PAINTSTRUCT ps;
		HDC dc = BeginPaint(w, &ps);
		FillRect(dc, &ps.rcPaint, (HBRUSH)GetStockObject(w == b ? GRAY_BRUSH : WHITE_BRUSH));
		EndPaint(w, &ps);
		return 0;
	}
	}
	return DefWindowProcA(w, m, wp, lp);
}

int main(int argc, char **argv)
{
	int secs = argc > 1 ? atoi(argv[1]) : 300;
	WNDCLASSA c = {0};
	c.lpfnWndProc = proc;
	c.lpszClassName = "kf";
	c.hCursor = LoadCursor(NULL, IDC_ARROW);
	RegisterClassA(&c);
	a = CreateWindowExA(0, "kf", "kf-a", WS_OVERLAPPEDWINDOW, 100, 100, 320, 240, NULL, NULL, NULL, NULL);
	/* A is activated while hidden, before Wine has a surface for it, then
	 * shown without activation: Win32 has it active, but the compositor was
	 * never told (winewayland sends activate only for a window that has its
	 * placement already), as with Momodora's window. */
	SetForegroundWindow(a);
	SetActiveWindow(a);
	ShowWindow(a, SW_SHOWNA);
	off = CreateWindowExA(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, "kf", "kf-off", WS_POPUP, -32000, -32000, 160, 40, NULL, NULL, NULL, NULL);
	ShowWindow(off, SW_SHOWNOACTIVATE);
	UpdateWindow(a);
	UpdateWindow(off);
	say("ready, foreground %s", name(GetForegroundWindow()));
	SetTimer(NULL, 0, secs * 1000, NULL);
	MSG m;
	while (GetMessageA(&m, NULL, 0, 0) > 0) {
		if (m.message == WM_TIMER && !m.hwnd) break;
		TranslateMessage(&m);
		DispatchMessageA(&m);
	}
	say("done");
	return 0;
}
