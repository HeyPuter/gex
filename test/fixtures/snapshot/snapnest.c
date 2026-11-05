/* A Windows x64 program for 64-bit Wine (test/snapshot/snapshot's snapnest case): a
 * thread waiting two levels deep in Wine. It makes a message-only window
 * whose WM_CREATE, which win32u sends from inside NtUserCreateWindowEx (a
 * callback into the guest), runs a message loop of its own, so its
 * GetMessage waits inside the outer call; a second thread waits in a
 * GetMessage of its own at the top level. The main thread sleeps 4 s,
 * timed by the performance counter, then posts both threads their
 * messages. A snapshot taken during the sleep and restored later must
 * print what a whole run prints. */
#include <windows.h>
#include <stdio.h>

static HANDLE ready[2];
static DWORD tids[2];

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
	if (msg == WM_CREATE) {
		MSG m;
		int sum = 0;
		PeekMessageA(&m, NULL, 0, 0, PM_NOREMOVE);
		SetEvent(ready[0]);
		while (GetMessageA(&m, NULL, 0, 0) > 0)
			if (m.message == WM_USER) sum += (int)m.wParam;
		printf("nested: WM_QUIT inside WM_CREATE after a sum of %d\n", sum);
		fflush(stdout);
		return 0;
	}
	return DefWindowProcA(hwnd, msg, wp, lp);
}

static DWORD WINAPI nested(LPVOID arg)
{
	WNDCLASSA wc = { 0 };
	wc.lpfnWndProc = proc;
	wc.hInstance = GetModuleHandleA(NULL);
	wc.lpszClassName = "snapnest";
	RegisterClassA(&wc);
	HWND w = CreateWindowExA(0, "snapnest", "", 0, 0, 0, 0, 0, HWND_MESSAGE, NULL, wc.hInstance, NULL);
	printf("nested: window %s\n", w ? "made" : "failed");
	fflush(stdout);
	if (w) DestroyWindow(w);
	return 0;
}

static DWORD WINAPI flat(LPVOID arg)
{
	MSG m;
	int sum = 0;
	PeekMessageA(&m, NULL, 0, 0, PM_NOREMOVE);
	SetEvent(ready[1]);
	while (GetMessageA(&m, NULL, 0, 0) > 0)
		if (m.message == WM_USER) sum += (int)m.wParam;
	printf("flat: WM_QUIT after a sum of %d\n", sum);
	fflush(stdout);
	return 0;
}

int main(void)
{
	LARGE_INTEGER f, a, b;
	ready[0] = CreateEventA(NULL, TRUE, FALSE, NULL);
	ready[1] = CreateEventA(NULL, TRUE, FALSE, NULL);
	HANDLE t[2];
	t[0] = CreateThread(NULL, 0, nested, NULL, 0, &tids[0]);
	t[1] = CreateThread(NULL, 0, flat, NULL, 0, &tids[1]);
	WaitForMultipleObjects(2, ready, TRUE, INFINITE);
	printf("main: both threads parked\n");
	fflush(stdout);
	QueryPerformanceFrequency(&f);
	QueryPerformanceCounter(&a);
	Sleep(4000);
	QueryPerformanceCounter(&b);
	double s = (double)(b.QuadPart - a.QuadPart) / (double)f.QuadPart;
	if (s >= 4.0 && s < 5.5) printf("main: slept 4 s\n");
	else printf("main: slept %.3f s\n", s);
	fflush(stdout);
	for (int i = 0; i < 2; i++) {
		PostThreadMessageA(tids[i], WM_USER, 2, 0);
		PostThreadMessageA(tids[i], WM_USER, 3, 0);
		PostThreadMessageA(tids[i], WM_QUIT, 0, 0);
	}
	WaitForMultipleObjects(2, t, TRUE, INFINITE);
	printf("main: done\n");
	return 0;
}
