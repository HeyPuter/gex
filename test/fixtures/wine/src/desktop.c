/* The desktop process (explorer.exe) up with its graphics driver: its tray
 * window exists and the screen is the display's, not the null driver's
 * 1024x768. A DLL's process attach in explorer that needs the user driver
 * before explorer loads its graphics driver (ieframe's window class, under the
 * native PE core's eager delay imports) once left explorer on the null
 * driver: no window could be created, no tray, and the display was the null
 * driver's.
 *
 *   desktop.exe WIDTH HEIGHT   (the display's size)
 * gex-nt-arch: x64
 */
#include <stdio.h>
#include <stdlib.h>
#include <windows.h>

int main(int argc, char **argv)
{
    int want_w = argc > 2 ? atoi(argv[1]) : 640, want_h = argc > 2 ? atoi(argv[2]) : 480;
    int failures = 0, w, h, i;
    HWND tray = 0, probe;

    /* a window of our own: the desktop process is started for it */
    probe = CreateWindowExW(0, L"STATIC", L"desktop probe", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, 0, 0, 0, 0);
    if (probe) printf("ok a window\n");
    else { printf("FAIL a window (error %lu)\n", GetLastError()); failures++; }

    for (i = 0; i < 200 && !tray; i++)
    {
        tray = FindWindowW(L"Shell_TrayWnd", NULL);
        if (!tray) Sleep(50);
    }
    if (tray) printf("ok the desktop process's tray window\n");
    else { printf("FAIL the desktop process's tray window\n"); failures++; }

    w = GetSystemMetrics(SM_CXSCREEN);
    h = GetSystemMetrics(SM_CYSCREEN);
    if (w == want_w && h == want_h) printf("ok the screen is %dx%d\n", w, h);
    else { printf("FAIL the screen is %dx%d, want %dx%d\n", w, h, want_w, want_h); failures++; }

    if (probe) DestroyWindow(probe);
    printf("%d failures\n", failures);
    return failures;
}
