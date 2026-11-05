/* A Windows x64 program for 64-bit Wine (test/snapshot/snapshot's snapwait case): one
 * thread in GetMessage, one in WaitForSingleObject, the main thread in a
 * 4 s Sleep timed by the performance counter; then the main thread posts
 * the first thread its messages and sets the second's event. A snapshot taken
 * during the sleep and restored later must print what a whole run prints:
 * the sleep's time on the restored counter is the 4 s the program asked
 * for, not that plus the time the snapshot was saved. */
#include <windows.h>
#include <stdio.h>

static HANDLE go, ready[2];

static DWORD WINAPI msgs(LPVOID arg)
{
	MSG m;
	int sum = 0;
	PeekMessageA(&m, NULL, 0, 0, PM_NOREMOVE);
	SetEvent(ready[0]);
	while (GetMessageA(&m, NULL, 0, 0) > 0)
		if (m.message == WM_USER) sum += (int)m.wParam;
	printf("msgs: WM_QUIT after a sum of %d\n", sum);
	fflush(stdout);
	return 0;
}

static DWORD WINAPI waits(LPVOID arg)
{
	SetEvent(ready[1]);
	DWORD r = WaitForSingleObject(go, INFINITE);
	printf("waits: %s\n", r == WAIT_OBJECT_0 ? "the event" : "failed");
	fflush(stdout);
	return 0;
}

int main(void)
{
	DWORD tid;
	LARGE_INTEGER f, a, b;
	go = CreateEventA(NULL, TRUE, FALSE, NULL);
	ready[0] = CreateEventA(NULL, TRUE, FALSE, NULL);
	ready[1] = CreateEventA(NULL, TRUE, FALSE, NULL);
	HANDLE t1 = CreateThread(NULL, 0, msgs, NULL, 0, &tid);
	HANDLE t2 = CreateThread(NULL, 0, waits, NULL, 0, NULL);
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
	PostThreadMessageA(tid, WM_USER, 2, 0);
	PostThreadMessageA(tid, WM_USER, 3, 0);
	PostThreadMessageA(tid, WM_QUIT, 0, 0);
	SetEvent(go);
	WaitForSingleObject(t1, INFINITE);
	WaitForSingleObject(t2, INFINITE);
	printf("main: done\n");
	return 0;
}
