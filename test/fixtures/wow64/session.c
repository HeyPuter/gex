/* The session mirror (docs/wineserver-kernel.md §6.1): win32u reads the
 * session's shared objects (a window's, the input state) from a copy of the
 * server's memory. Another process's change must show at once: the parent
 * makes a window, a child sees it, the parent destroys it, the child must
 * see it gone on its next look (not after a TTL), and a run of IsWindow
 * costs no requests. Prints "session ok" and the IsWindow cost.
 *
 *   session.exe            the parent
 *   session.exe HWND READY GONE   the child */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static double now_us(void)
{
    LARGE_INTEGER c, f;
    QueryPerformanceCounter(&c); QueryPerformanceFrequency(&f);
    return (double)c.QuadPart * 1e6 / (double)f.QuadPart;
}

static int child(HWND hwnd, HANDLE ready, HANDLE gone)
{
    int i, n = 20000, alive = 0;
    double t0;
    if (!IsWindow(hwnd)) { printf("session: child does not see the window\n"); return 1; }
    t0 = now_us();
    for (i = 0; i < n; i++) alive += !!IsWindow(hwnd);
    printf("session: IsWindow %.2f us each (%d of %d)\n", (now_us() - t0) / n, alive, n);
    SetEvent(ready);
    WaitForSingleObject(gone, INFINITE);  /* the parent destroyed it */
    if (IsWindow(hwnd)) { printf("session: child still sees the destroyed window\n"); return 1; }
    return 0;
}

int main(int argc, char **argv)
{
    char cmd[MAX_PATH + 64], self[MAX_PATH];
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    SECURITY_ATTRIBUTES sa = { sizeof(sa), NULL, TRUE };
    HANDLE ready, gone;
    HWND hwnd;
    DWORD rc;

    if (argc > 3) return child((HWND)(ULONG_PTR)strtoul(argv[1], NULL, 0), (HANDLE)(ULONG_PTR)strtoul(argv[2], NULL, 0),
                               (HANDLE)(ULONG_PTR)strtoul(argv[3], NULL, 0));
    hwnd = CreateWindowA("STATIC", "session", WS_POPUP, 0, 0, 10, 10, NULL, NULL, NULL, NULL);
    if (!hwnd) { printf("session: no window\n"); return 1; }
    ready = CreateEventA(&sa, TRUE, FALSE, NULL);
    gone = CreateEventA(&sa, TRUE, FALSE, NULL);
    GetModuleFileNameA(NULL, self, sizeof(self));
    sprintf(cmd, "\"%s\" %#lx %#lx %#lx", self, (unsigned long)(ULONG_PTR)hwnd, (unsigned long)(ULONG_PTR)ready,
            (unsigned long)(ULONG_PTR)gone);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) { printf("session: no child\n"); return 1; }
    if (WaitForMultipleObjects(2, (HANDLE[]){ ready, pi.hProcess }, FALSE, INFINITE) != WAIT_OBJECT_0)
    { printf("session: child ended early\n"); return 1; }
    DestroyWindow(hwnd);
    SetEvent(gone);
    WaitForSingleObject(pi.hProcess, INFINITE);
    GetExitCodeProcess(pi.hProcess, &rc);
    if (rc) { printf("session: child failed (%lu)\n", rc); return 1; }
    printf("session ok\n");
    return 0;
}
