/* Wheel input to a Win32 window (tests/wheel/wheel.sh): a window that
 * counts and sums WM_MOUSEWHEEL and WM_MOUSEHWHEEL, driven by the
 * compositor's scripted scrolls (wheel.wl):
 *   1. one mouse notch down (100 px): WM_MOUSEWHEEL -120;
 *   2. a trackpad's 40 deltas of 2.5 px down (100 px): WM_MOUSEWHEELs adding
 *      up to another -120, none rounded away (each delta alone is 3);
 *   3. one notch right: WM_MOUSEHWHEEL adding up to +120.
 * Checked by the sums: Wine merges wheel messages still queued, so a slow
 * program may see the notch and the start of the trackpad's stream as one.
 * Prints every message's delta, then "wheel ok" (or what was missing) when
 * the scrolls are in, or after SECONDS (default 240). Exit 0 on ok. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

#ifndef WM_MOUSEHWHEEL
#define WM_MOUSEHWHEEL 0x020e
#endif

static int vn, vsum, hn, hsum, first;

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    short d = GET_WHEEL_DELTA_WPARAM(wp);
    switch (msg)
    {
    case WM_MOUSEWHEEL:
        if (!vn++) first = d;
        vsum += d;
        printf("wheel %d (sum %d)\n", d, vsum);
        return 0;
    case WM_MOUSEHWHEEL:
        hn++; hsum += d;
        printf("hwheel %d (sum %d)\n", d, hsum);
        return 0;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

int main(int argc, char **argv)
{
    int secs = argc > 1 ? atoi(argv[1]) : 240;
    WNDCLASSA wc = {0};
    DWORD end;
    HWND hwnd;
    MSG m;
    int ok;

    setvbuf(stdout, NULL, _IONBF, 0);
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.hbrBackground = GetStockObject(WHITE_BRUSH);
    wc.hCursor = LoadCursorA(NULL, (LPCSTR)IDC_ARROW);
    wc.lpszClassName = "wheel";
    RegisterClassA(&wc);
    hwnd = CreateWindowA("wheel", "wheel", WS_POPUP | WS_VISIBLE, 0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN), NULL, NULL, wc.hInstance, NULL);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    printf("wheel: window up\n");
    end = GetTickCount() + secs * 1000;
    while ((int)(end - GetTickCount()) > 0)
    {
        if (vsum == -240 && hsum == 120) break;
        MsgWaitForMultipleObjects(0, NULL, FALSE, 100, QS_ALLINPUT);
        while (PeekMessageA(&m, NULL, 0, 0, PM_REMOVE)) { TranslateMessage(&m); DispatchMessageA(&m); }
    }
    ok = vsum == -240 && hsum == 120;
    printf("wheel: vertical %d messages sum %d (first %d), horizontal %d sum %d\n",
           vn, vsum, first, hn, hsum);
    printf(ok ? "wheel ok\n" : "wheel FAIL\n");
    DestroyWindow(hwnd);
    return !ok;
}
