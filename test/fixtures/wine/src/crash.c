/* gex-nt-libs: -luser32 */
/* A program that crashes (test/wow64/crash, test/wine/crash): a window, shown, then argv[1]'s
 * crash, with no handler of its own: "h" a write to 0xfffff000 (an access
 * violation: no page there), "u" an illegal instruction (ud2), "s" a read
 * at 0xfffff000 inside a C runtime function (Wine's msvcrt, native code on
 * gex). Prints "crash now" first; never "crash survived". */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp)
{
    return DefWindowProcA(w, m, wp, lp);
}

int main(int argc, char **argv)
{
    WNDCLASSA wc = { 0 };
    HWND h;
    MSG msg;
    DWORD t;

    setvbuf(stdout, NULL, _IONBF, 0);
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "crash";
    RegisterClassA(&wc);
    h = CreateWindowA("crash", "crash", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0, 200, 200, NULL, NULL, wc.hInstance, NULL);
    printf("crash window %p\n", h);
    /* a second of messages, so the window is drawn */
    for (t = GetTickCount(); GetTickCount() - t < 1000; Sleep(10))
        while (PeekMessageA(&msg, 0, 0, 0, PM_REMOVE)) DispatchMessageA(&msg);
    printf("crash now\n");
    switch (argc > 1 ? argv[1][0] : 'h')
    {
    case 'h': *(volatile int *)(UINT_PTR)0xfffff000u = 1; break;
    case 'u': __asm__ volatile("ud2"); break;
    case 's': printf("%u\n", (unsigned)strlen((const char *)(UINT_PTR)0xfffff000u)); break;
    }
    printf("crash survived\n");
    return 0;
}
