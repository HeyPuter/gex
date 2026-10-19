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
 * the scrolls are in, or after SECONDS (default 240). Exit 0 on ok.
 *
 * `wheel.exe SECONDS slow` (wheel-slow.wl) is an application that is slow
 * with its input: a WH_MOUSE hook sleeps on every wheel message, which is
 * after the server handed it over and before win32u accepts it. A wheel
 * event arriving then must not be merged into that message (the app
 * already has its delta), or the sum comes up short. A burst of 20 moves
 * that arrives while the hook sleeps on the first must still coalesce
 * (fewer than 10 WM_MOUSEMOVE). */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef WM_MOUSEHWHEEL
#define WM_MOUSEHWHEEL 0x020e
#endif

static int vn, vsum, hn, hsum, first, slow, moves, burst, last_x;

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
    case WM_MOUSEMOVE:
        moves++;
        last_x = (short)LOWORD(lp);
        if (last_x > 400) burst++;
        return 0;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

/* The slow application: sleeps while a wheel message, or the burst's
 * first move, is between the server's hand-over and win32u's accept. */
static LRESULT CALLBACK mouse_hook(int code, WPARAM wp, LPARAM lp)
{
    static int burst_slept;
    if (code == HC_ACTION && wp == WM_MOUSEWHEEL) Sleep(60);
    if (code == HC_ACTION && wp == WM_MOUSEMOVE && !burst_slept &&
        ((MOUSEHOOKSTRUCT *)lp)->pt.x > 400) { burst_slept = 1; Sleep(500); }
    return CallNextHookEx(NULL, code, wp, lp);
}

int main(int argc, char **argv)
{
    int secs = argc > 1 ? atoi(argv[1]) : 240;
    slow = argc > 2 && !strcmp(argv[2], "slow");
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
    if (slow) SetWindowsHookExA(WH_MOUSE, mouse_hook, NULL, GetCurrentThreadId());
    printf("wheel: window up\n");
    end = GetTickCount() + secs * 1000;
    while ((int)(end - GetTickCount()) > 0)
    {
        if (vsum == -240 && hsum == 120 && (!slow || last_x == 590)) break;
        MsgWaitForMultipleObjects(0, NULL, FALSE, 100, QS_ALLINPUT);
        while (PeekMessageA(&m, NULL, 0, 0, PM_REMOVE)) { TranslateMessage(&m); DispatchMessageA(&m); }
    }
    ok = vsum == -240 && hsum == 120 && (!slow || (last_x == 590 && burst < 10));
    printf("wheel: vertical %d messages sum %d (first %d), horizontal %d sum %d",
           vn, vsum, first, hn, hsum);
    if (slow) printf(", moves %d (burst of 20 as %d, last x %d)", moves, burst, last_x);
    printf("\n");
    printf(ok ? "wheel ok\n" : "wheel FAIL\n");
    DestroyWindow(hwnd);
    return !ok;
}
