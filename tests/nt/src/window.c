/* A window that stays up: four colored quadrants and a white square in the
 * middle, painted through WM_PAINT. A click prints "click X Y" and leaves a
 * black mark there; a key prints "key VK"; Escape or closing the window ends
 * it, as does the argument's number of seconds (default 30). Prints "shown"
 * once the first paint is done, and "bye" at the end. What a display (the
 * page's compositor, or --wayland-out) is checked against.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld -lgdi32 */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static POINT marks[64];
static int nmarks;

static void fill(HDC dc, int l, int t, int r, int b, COLORREF c) {
    RECT rc = {l, t, r, b};
    HBRUSH br = CreateSolidBrush(c);
    FillRect(dc, &rc, br);
    DeleteObject(br);
}

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    switch (m) {
    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(w, &ps);
        RECT rc;
        GetClientRect(w, &rc);
        int cx = rc.right / 2, cy = rc.bottom / 2;
        fill(dc, 0, 0, cx, cy, RGB(220, 40, 40));
        fill(dc, cx, 0, rc.right, cy, RGB(40, 200, 60));
        fill(dc, 0, cy, cx, rc.bottom, RGB(40, 80, 220));
        fill(dc, cx, cy, rc.right, rc.bottom, RGB(230, 210, 40));
        fill(dc, cx - 20, cy - 20, cx + 20, cy + 20, RGB(255, 255, 255));
        for (int i = 0; i < nmarks; i++)
            fill(dc, marks[i].x - 4, marks[i].y - 4, marks[i].x + 4, marks[i].y + 4, RGB(0, 0, 0));
        EndPaint(w, &ps);
        return 0;
    }
    case WM_LBUTTONDOWN: {
        int x = (short)LOWORD(lp), y = (short)HIWORD(lp);
        printf("click %d %d\n", x, y);
        fflush(stdout);
        if (nmarks < 64) marks[nmarks++] = (POINT){x, y};
        InvalidateRect(w, 0, FALSE);
        return 0;
    }
    case WM_KEYDOWN:
        printf("key %u\n", (unsigned)wp);
        fflush(stdout);
        if (wp == VK_ESCAPE) DestroyWindow(w);
        return 0;
    case WM_TIMER:
        DestroyWindow(w);
        return 0;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(w, m, wp, lp);
}

int main(int argc, char **argv) {
    int secs = argc > 1 ? atoi(argv[1]) : 30;
    WNDCLASSW wc = {0};
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.hCursor = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    wc.lpszClassName = L"gex_window";
    RegisterClassW(&wc);
    HWND w = CreateWindowExW(0, L"gex_window", L"gex window", WS_POPUP, 100, 100, 320, 240, 0, 0, wc.hInstance, 0);
    ShowWindow(w, SW_SHOW);
    UpdateWindow(w);
    SetTimer(w, 1, secs * 1000, 0);
    printf("shown\n");
    fflush(stdout);
    MSG msg;
    while (GetMessageW(&msg, 0, 0, 0) > 0) {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    printf("bye\n");
    return 0;
}
