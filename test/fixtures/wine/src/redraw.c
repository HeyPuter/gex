/* A shown window stays mapped while WM_SETREDRAW has cleared its WS_VISIBLE
 * (PuTTY's configuration dialog filling a page): moved meanwhile, it was
 * unmapped and mapped again with only what was repainted after, its frame
 * (here green, drawn by its own WM_NCPAINT; the client red) gone.
 * gex-nt-arch: x64
 * gex-nt-libs: -lgdi32
 */
#include <windows.h>
#include <stdio.h>

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    switch (msg)
    {
    case WM_NCPAINT:
    {
        HDC dc = GetWindowDC(hwnd);
        RECT r;
        HBRUSH green = CreateSolidBrush(RGB(40, 200, 60));

        GetWindowRect(hwnd, &r);
        OffsetRect(&r, -r.left, -r.top);
        FillRect(dc, &r, green);
        DeleteObject(green);
        ReleaseDC(hwnd, dc);
        return 0;
    }
    case WM_NCACTIVATE:
        return TRUE;
    case WM_ERASEBKGND:
    {
        RECT r;
        HBRUSH red = CreateSolidBrush(RGB(220, 40, 40));

        GetClientRect(hwnd, &r);
        FillRect((HDC)wp, &r, red);
        DeleteObject(red);
        return 1;
    }
    }
    return DefWindowProcW(hwnd, msg, wp, lp);
}

static void pump(DWORD ms)
{
    DWORD end = GetTickCount() + ms;
    MSG msg;

    while ((int)(end - GetTickCount()) > 0)
    {
        while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
        Sleep(20);
    }
}

int main(void)
{
    WNDCLASSW wc = { 0, proc, 0, 0, GetModuleHandleW(NULL), 0, LoadCursorW(0, (LPCWSTR)IDC_ARROW), 0, 0, L"redraw" };
    HWND hwnd;

    RegisterClassW(&wc);
    hwnd = CreateWindowExW(0, L"redraw", L"redraw", WS_OVERLAPPEDWINDOW, 40, 40, 300, 200, 0, 0, wc.hInstance, 0);
    ShowWindow(hwnd, SW_SHOW);
    UpdateWindow(hwnd);
    pump(1500);
    SendMessageW(hwnd, WM_SETREDRAW, FALSE, 0);
    SetWindowPos(hwnd, 0, 41, 41, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    pump(300);
    SendMessageW(hwnd, WM_SETREDRAW, TRUE, 0);
    InvalidateRect(hwnd, NULL, TRUE);
    pump(1500);
    printf("%s\n", IsWindowVisible(hwnd) ? "visible" : "hidden");
    DestroyWindow(hwnd);
    return 0;
}
