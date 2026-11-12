/* The shell desktop's taskbar is on top of other windows, and a full-screen
 * foreground window covers it (test/wine64/tbtop), as on Windows. The taskbar
 * (Shell_TrayWnd) is WS_EX_TOPMOST. A normal window W placed over it and made
 * foreground stays under it. A full-screen popup F covers it while F is
 * foreground, and when W is foreground again the taskbar is back on top.
 * "Under it" is what the pointer finds there: WindowFromPoint at the middle
 * of the part of the taskbar W covers. Prints "ok"/"FAIL" lines and
 * "N failures".
 *
 *   tbtop.exe
 * gex-nt-arch: x64
 * gex-nt-libs: -lgdi32
 */
#include <windows.h>
#include <stdio.h>

/* Each line in one write, as deco.c's. */
#define printf(...) do { char line_[256]; snprintf(line_, sizeof(line_), __VA_ARGS__); fputs(line_, stdout); fflush(stdout); } while (0)

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    if (msg == WM_PAINT)
    {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(hwnd, &ps);
        FillRect(dc, &ps.rcPaint, GetStockObject(GRAY_BRUSH));
        EndPaint(hwnd, &ps);
        return 0;
    }
    return DefWindowProcW(hwnd, msg, wp, lp);
}

static void pump(DWORD ms)
{
    DWORD end = GetTickCount() + ms;
    MSG msg;
    while ((int)(end - GetTickCount()) > 0)
    {
        while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) { TranslateMessage(&msg); DispatchMessageW(&msg); }
        MsgWaitForMultipleObjects(0, NULL, FALSE, 20, QS_ALLINPUT);
    }
}

/* the top-level window the pointer finds at pt */
static HWND at(POINT pt)
{
    HWND h = WindowFromPoint(pt);
    return h ? GetAncestor(h, GA_ROOT) : 0;
}

/* whether at(pt) is want within secs (the taskbar reorders itself on a posted notification) */
static BOOL becomes(POINT pt, HWND want, int secs)
{
    int i;
    for (i = 0; i < secs * 20; i++)
    {
        if (at(pt) == want) return TRUE;
        pump(50);
    }
    return FALSE;
}

static int failures;
static void check(BOOL ok, const char *what)
{
    printf("%s %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) failures++;
}

int main(void)
{
    WNDCLASSW wc = {0};
    HWND tray, w, f;
    RECT tr;
    POINT pt;
    int i, cx = GetSystemMetrics(SM_CXSCREEN), cy = GetSystemMetrics(SM_CYSCREEN);

    /* the default desktop, "shell", comes up with this program's first window */
    CreateWindowExW(0, L"STATIC", L"", WS_POPUP, 0, 0, 1, 1, 0, 0, 0, 0);
    for (i = 0; i < 600 && !(tray = FindWindowW(L"Shell_TrayWnd", NULL)); i++) Sleep(50);
    if (!tray) { printf("FAIL no taskbar\n1 failures\n"); return 1; }
    GetWindowRect(tray, &tr);
    printf("tbtop: taskbar %ldx%ld at %ld,%ld, screen %dx%d\n", tr.right - tr.left, tr.bottom - tr.top, tr.left, tr.top, cx, cy);
    check((GetWindowLongW(tray, GWL_EXSTYLE) & WS_EX_TOPMOST) != 0, "the taskbar is WS_EX_TOPMOST");

    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    wc.lpszClassName = L"tbtop";
    RegisterClassW(&wc);
    /* W: a normal window over the left of the taskbar, foreground */
    w = CreateWindowExW(0, L"tbtop", L"W", WS_POPUP | WS_VISIBLE, tr.left + 100, tr.top - 150, 300, 200, 0, 0, 0, 0);
    SetForegroundWindow(w);
    pump(500);
    pt.x = tr.left + 250; pt.y = (tr.top + tr.bottom) / 2;
    check(GetForegroundWindow() == w, "W is foreground");
    check(becomes(pt, tray, 3), "a normal foreground window placed over the taskbar stays under it");

    /* F: full-screen, foreground: over the taskbar */
    f = CreateWindowExW(0, L"tbtop", L"F", WS_POPUP | WS_VISIBLE, 0, 0, cx, cy, 0, 0, 0, 0);
    SetForegroundWindow(f);
    check(becomes(pt, f, 3), "a full-screen foreground window covers the taskbar");

    /* W foreground again: the taskbar is back on top, over W and F */
    SetForegroundWindow(w);
    check(GetForegroundWindow() == w, "W is foreground again");
    check(becomes(pt, tray, 3), "with the full-screen window no longer foreground the taskbar is on top again");
    pt.x = tr.right - 50;
    check(becomes(pt, tray, 3), "and over the full-screen window too");

    printf("%d failures\n", failures);
    DestroyWindow(f);
    DestroyWindow(w);
    return failures;
}
