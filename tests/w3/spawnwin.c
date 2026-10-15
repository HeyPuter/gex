/* A Wayland client that spawns processes, with a thread of its own waiting:
 * the parent's windows must keep working after each spawn.
 *
 * Under W3 a Windows process's Unix child is a CLONE_VM|CLONE_VFORK clone,
 * which the page runs in the cloning thread's Worker until its exec. The
 * child's kernel was built with the per-process devices, and its compositor
 * replaced the parent's for good (the compositor is a static of the memory,
 * which is the parent's): the parent's Wayland clients were never pumped
 * again, so its next window waited forever on its first round trip
 * (winlogon after it spawned explorer /desktop, in every shell boot).
 *
 * The parent makes a window, starts a thread that waits on an event, then
 * N times: starts itself as a child, waits for it, makes and shows another
 * window and pumps its messages, and sets the event for the thread, which
 * answers on another. Prints "spawnwin ok N" and exits 0. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static HANDLE go, done;
static volatile LONG woken;

static DWORD WINAPI waiter(void *arg)
{
    for (;;)
    {
        if (WaitForSingleObject(go, INFINITE)) return 1;
        if (InterlockedIncrement(&woken) < 0) return 0;
        SetEvent(done);
    }
}

static LRESULT CALLBACK wndproc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    if (msg == WM_PAINT)
    {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(hwnd, &ps);
        FillRect(dc, &ps.rcPaint, GetStockObject(WHITE_BRUSH));
        EndPaint(hwnd, &ps);
        return 0;
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

static HWND window(int i)
{
    char title[32];
    MSG m;
    HWND hwnd;

    snprintf(title, sizeof(title), "spawnwin %d", i);
    hwnd = CreateWindowExA(0, "spawnwin", title, WS_OVERLAPPEDWINDOW, 20 + i * 8, 20 + i * 8,
                           200, 120, NULL, NULL, GetModuleHandleA(NULL), NULL);
    if (!hwnd) return NULL;
    ShowWindow(hwnd, SW_SHOW);
    UpdateWindow(hwnd);
    SetWindowPos(hwnd, HWND_TOP, 30 + i * 8, 30 + i * 8, 220, 140, SWP_SHOWWINDOW);
    while (PeekMessageA(&m, NULL, 0, 0, PM_REMOVE))
    {
        TranslateMessage(&m);
        DispatchMessageA(&m);
    }
    return hwnd;
}

int main(int argc, char **argv)
{
    char self[MAX_PATH], cmd[MAX_PATH + 16];
    WNDCLASSA wc = {0};
    HANDLE thread;
    int n, i;

    if (argc > 1 && !strcmp(argv[1], "child")) return 7;
    n = argc > 1 ? atoi(argv[1]) : 4;

    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.hCursor = LoadCursorA(NULL, (LPCSTR)IDC_ARROW);
    wc.lpszClassName = "spawnwin";
    if (!RegisterClassA(&wc)) { printf("RegisterClass failed %lu\n", GetLastError()); return 1; }
    if (!window(0)) { printf("first window failed %lu\n", GetLastError()); return 1; }

    go = CreateEventA(NULL, FALSE, FALSE, NULL);
    done = CreateEventA(NULL, FALSE, FALSE, NULL);
    thread = CreateThread(NULL, 0, waiter, NULL, 0, NULL);
    GetModuleFileNameA(NULL, self, sizeof(self));
    snprintf(cmd, sizeof(cmd), "\"%s\" child", self);

    for (i = 1; i <= n; i++)
    {
        STARTUPINFOA si = {sizeof(si)};
        PROCESS_INFORMATION pi;
        DWORD code = 0;

        if (!CreateProcessA(self, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi))
        {
            printf("CreateProcess %d failed %lu\n", i, GetLastError());
            return 1;
        }
        if (WaitForSingleObject(pi.hProcess, 60000) || !GetExitCodeProcess(pi.hProcess, &code) || code != 7)
        {
            printf("child %d: exit %lu\n", i, code);
            return 1;
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        /* The parent's Wayland client after the spawn: a new window, shown. */
        if (!window(i)) { printf("window %d failed %lu\n", i, GetLastError()); return 1; }
        SetEvent(go);
        if (WaitForSingleObject(done, 60000)) { printf("thread did not answer %d\n", i); return 1; }
    }
    if (woken != n) { printf("woken %ld of %d\n", woken, n); return 1; }
    printf("spawnwin ok %d\n", n);
    fflush(stdout);
    ExitProcess(0);
}
