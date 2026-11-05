/* Faults nested in their own dispatch without end: a vectored handler that
 * faults on every call ("plain"), or one whose fault comes from a window
 * procedure it sends a message to, so each level also nests a callback
 * ("callback"). On Windows the nest goes on until the thread's stack is
 * spent; the process ends with a stack overflow, never a hang. Prints
 * "nest N" at every level (the last is the deepest reached). Both arches
 * (the x86 one for 32-bit Wine). */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static volatile LONG n;
static HWND wnd;

static LRESULT CALLBACK wp(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_USER) __builtin_trap();
    return DefWindowProcA(h, m, w, l);
}

static LONG CALLBACK veh(EXCEPTION_POINTERS *e)
{
    LONG k = InterlockedIncrement(&n);
    printf("nest %ld\n", k); fflush(stdout);
    if (wnd) SendMessageA(wnd, WM_USER, 0, 0);
    else __builtin_trap();
    return EXCEPTION_CONTINUE_SEARCH;
}

int main(int argc, char **argv)
{
    int cb = argc > 1 && !strcmp(argv[1], "callback");
    if (cb)
    {
        WNDCLASSA c = { .lpfnWndProc = wp, .hInstance = GetModuleHandleA(0), .lpszClassName = "faultnest" };
        RegisterClassA(&c);
        wnd = CreateWindowA("faultnest", "", 0, 0, 0, 1, 1, HWND_MESSAGE, 0, c.hInstance, 0);
    }
    printf("fault-nest %s start\n", cb ? "callback" : "plain"); fflush(stdout);
    AddVectoredExceptionHandler(1, veh);
    if (wnd) SendMessageA(wnd, WM_USER, 0, 0);
    else __builtin_trap();
    printf("fault-nest survived\n");
    return 0;
}
