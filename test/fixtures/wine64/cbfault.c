/* cbfault.exe: a wild read in native code under a user callback (a window
 * procedure SendMessageW reaches through KeUserModeCallback, a
 * WH_CALLWNDPROC hook in the thread) is an access violation the process's
 * vectored handler sees, with the exception frames of the callback's dead
 * native frames gone; then dispatch goes on, and nothing calls a clobbered
 * frame's handler. Prints "cbfault: veh c0000005" and the dispatcher's end. */
#include <windows.h>
#include <stdio.h>
static volatile int *wild;
static int seen;
static LONG CALLBACK veh(EXCEPTION_POINTERS *ep)
{
    if (!seen++) { printf("cbfault: veh %08lx\n", ep->ExceptionRecord->ExceptionCode); fflush(stdout); }
    return EXCEPTION_CONTINUE_SEARCH;
}
static LRESULT CALLBACK cwp(int code, WPARAM w, LPARAM l) { return CallNextHookEx(NULL, code, w, l); }
static LRESULT WINAPI proc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_USER) return *wild + 1;
    return DefWindowProcW(h, m, w, l);
}
int main(int argc, char **argv)
{
    WNDCLASSW wc = {0};
    HWND w;
    LRESULT r;
    wild = (volatile int *)(ULONG_PTR)(argc > 5 ? 0 : 0xfffffff0u);
    wc.lpfnWndProc = proc; wc.hInstance = GetModuleHandleW(NULL); wc.lpszClassName = L"cbfault";
    RegisterClassW(&wc);
    w = CreateWindowW(L"cbfault", L"", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, NULL, NULL, wc.hInstance, NULL);
    /* (through GetProcAddress: a core linked with only the exports Wine's own
     * modules import has no AddVectoredExceptionHandler to bind) */
    ((PVOID (WINAPI *)(ULONG, PVECTORED_EXCEPTION_HANDLER))GetProcAddress(GetModuleHandleA("kernel32.dll"),
                                                                       "AddVectoredExceptionHandler"))(1, veh);
    SetWindowsHookExW(WH_CALLWNDPROC, cwp, NULL, GetCurrentThreadId());
    printf("cbfault: sending\n"); fflush(stdout);
    r = SendMessageW(w, WM_USER, 0, 0);
    printf("cbfault: returned %lld, veh %d\n", (long long)r, seen);
    fflush(stdout);
    return 0;
}
