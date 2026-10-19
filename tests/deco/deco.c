/* Window decorations under gex's compositor (tests/deco/deco.sh): Wine draws
 * a window's frame and the compositor draws none, as on Windows.
 *   P  a WS_POPUP window at 100,100, 300x200, red, its top 30 px a yellow
 *      caption of its own (WM_NCHITTEST HTCAPTION), as FL Studio draws one;
 *   O  a WS_OVERLAPPEDWINDOW at 500,100, 300x200 (Wine's caption and
 *      border), blue inside.
 *   C  a WS_POPUP window centred on the screen (GetSystemMetrics), 200x100,
 *      green, as FL Studio centres its splash; after 10 s SetWindowPos moves
 *      it to 20,600.
 * The script (deco.wl) drags P by its caption by +200,+150; Win32's move
 * loop moves it. Prints the windows' rectangles, then "deco ok" when P is at
 * 300,250, or "deco FAIL" after SECONDS (default 240). */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static HWND p, o, c;
static int moved_c;

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    switch (msg)
    {
    case WM_NCHITTEST:
        if (hwnd == p)
        {
            POINT pt = { (short)LOWORD(lp), (short)HIWORD(lp) };
            ScreenToClient(hwnd, &pt);
            return pt.y < 30 ? HTCAPTION : HTCLIENT;
        }
        break;
    case WM_PAINT:
    {
        PAINTSTRUCT ps;
        RECT r;
        HDC dc = BeginPaint(hwnd, &ps);
        HBRUSH b;
        GetClientRect(hwnd, &r);
        b = CreateSolidBrush(hwnd == p ? RGB(255, 0, 0) : hwnd == c ? RGB(0, 255, 0) : RGB(0, 0, 255));
        FillRect(dc, &r, b); DeleteObject(b);
        if (hwnd == p)
        {
            r.bottom = 30;
            b = CreateSolidBrush(RGB(255, 255, 0));
            FillRect(dc, &r, b); DeleteObject(b);
        }
        EndPaint(hwnd, &ps);
        return 0;
    }
    case WM_TIMER:
        KillTimer(hwnd, 1);
        SetWindowPos(c, NULL, 20, 600, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        InvalidateRect(c, NULL, TRUE);
        printf("C moved to 20,600\n");
        moved_c = 1;
        return 0;
    case WM_EXITSIZEMOVE:
    {
        RECT r;
        GetWindowRect(hwnd, &r);
        printf("moved %s to %ld,%ld\n", hwnd == p ? "P" : "O", r.left, r.top);
        /* a new frame, so the compositor's dump lists where it is now */
        InvalidateRect(hwnd, NULL, TRUE);
        return 0;
    }
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

int main(int argc, char **argv)
{
    int secs = argc > 1 ? atoi(argv[1]) : 240;
    WNDCLASSA wc = {0};
    RECT r;
    DWORD end;
    MSG m;
    int ok = 0;

    setvbuf(stdout, NULL, _IONBF, 0);
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.hCursor = LoadCursorA(NULL, (LPCSTR)IDC_ARROW);
    wc.lpszClassName = "deco";
    RegisterClassA(&wc);
    o = CreateWindowA("deco", "deco O", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 500, 100, 300, 200, NULL, NULL, wc.hInstance, NULL);
    p = CreateWindowA("deco", "deco P", WS_POPUP | WS_VISIBLE, 100, 100, 300, 200, NULL, NULL, wc.hInstance, NULL);
    c = CreateWindowA("deco", "deco C", WS_POPUP | WS_VISIBLE, (GetSystemMetrics(SM_CXSCREEN) - 200) / 2,
                      (GetSystemMetrics(SM_CYSCREEN) - 100) / 2, 200, 100, NULL, NULL, wc.hInstance, NULL);
    SetTimer(c, 1, 10000, NULL);
    GetWindowRect(p, &r); printf("P %ld,%ld %ldx%ld\n", r.left, r.top, r.right - r.left, r.bottom - r.top);
    GetWindowRect(o, &r); printf("O %ld,%ld %ldx%ld\n", r.left, r.top, r.right - r.left, r.bottom - r.top);
    GetWindowRect(c, &r); printf("C %ld,%ld %ldx%ld\n", r.left, r.top, r.right - r.left, r.bottom - r.top);
    end = GetTickCount() + secs * 1000;
    while ((int)(end - GetTickCount()) > 0)
    {
        GetWindowRect(p, &r);
        if (r.left == 300 && r.top == 250 && moved_c) { ok = 1; break; }
        MsgWaitForMultipleObjects(0, NULL, FALSE, 100, QS_ALLINPUT);
        while (PeekMessageA(&m, NULL, 0, 0, PM_REMOVE)) { TranslateMessage(&m); DispatchMessageA(&m); }
    }
    GetWindowRect(p, &r);
    printf("P at %ld,%ld\n", r.left, r.top);
    /* a few frames more for the dump to show it there */
    end = GetTickCount() + 3000;
    while ((int)(end - GetTickCount()) > 0)
    {
        MsgWaitForMultipleObjects(0, NULL, FALSE, 100, QS_ALLINPUT);
        while (PeekMessageA(&m, NULL, 0, 0, PM_REMOVE)) { TranslateMessage(&m); DispatchMessageA(&m); }
    }
    printf(ok ? "deco ok\n" : "deco FAIL\n");
    return !ok;
}
