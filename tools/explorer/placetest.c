/* placetest: windows at known Win32 positions, for the window placement leg
 * (placement-leg.sh). Four borderless windows of solid colour:
 *
 *   D  yellow  0,H-40  W x 40   topmost  (a taskbar's place)
 *   C  blue    250,120 120x80   topmost  (made first, so only its layer keeps it on top)
 *   A  red     100,100 200x150
 *   B  green   200,150 200x150  (made after A: above it)
 *
 * A click turns a window white and is logged to C:\placetest.log with the
 * point it landed on, so the leg can tell which window a click reached. A
 * click activates the window, and Win32 raises it: the red one clicked comes
 * up over the green one.
 *
 * Build: i686-w64-mingw32-gcc -O2 -o placetest.exe placetest.c -mwindows */
#include <windows.h>
#include <stdio.h>

static FILE *logf;

struct win { const char *name; COLORREF colour; int x, y, w, h; DWORD exstyle; HWND hwnd; BOOL clicked; };
static struct win wins[] = {
    { "C", RGB(0, 0, 255), 250, 120, 120, 80, WS_EX_TOPMOST },
    { "A", RGB(255, 0, 0), 100, 100, 200, 150, 0 },
    { "B", RGB(0, 255, 0), 200, 150, 200, 150, 0 },
    { "D", RGB(255, 255, 0), 0, -40, -1, 40, WS_EX_TOPMOST | WS_EX_TOOLWINDOW },
};

static struct win *find(HWND hwnd)
{
    for (int i = 0; i < 4; i++) if (wins[i].hwnd == hwnd) return &wins[i];
    return NULL;
}

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    struct win *w = find(hwnd);
    switch (msg)
    {
    case WM_PAINT:
    {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(hwnd, &ps);
        HBRUSH b = CreateSolidBrush(w && w->clicked ? RGB(255, 255, 255) : w ? w->colour : 0);
        FillRect(dc, &ps.rcPaint, b);
        DeleteObject(b);
        EndPaint(hwnd, &ps);
        return 0;
    }
    case WM_LBUTTONDOWN:
        if (w)
        {
            POINT pt = { (short)LOWORD(lp), (short)HIWORD(lp) };
            ClientToScreen(hwnd, &pt);
            fprintf(logf, "click %s at %ld,%ld\n", w->name, pt.x, pt.y);
            fflush(logf);
            w->clicked = TRUE;
            InvalidateRect(hwnd, NULL, TRUE);
        }
        return 0;
    case WM_ACTIVATE:
        if (w && LOWORD(wp) != WA_INACTIVE) { fprintf(logf, "active %s\n", w->name); fflush(logf); }
        break;
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

int WINAPI WinMain(HINSTANCE inst, HINSTANCE prev, LPSTR cmd, int show)
{
    WNDCLASSA wc = { 0 };
    int sw = GetSystemMetrics(SM_CXSCREEN), sh = GetSystemMetrics(SM_CYSCREEN);
    MSG msg;

    logf = fopen("C:\\placetest.log", "w");
    wc.lpfnWndProc = proc;
    wc.hInstance = inst;
    wc.hCursor = LoadCursorA(NULL, (LPCSTR)IDC_ARROW);
    wc.lpszClassName = "placetest";
    RegisterClassA(&wc);
    fprintf(logf, "screen %dx%d\n", sw, sh);
    for (int i = 0; i < 4; i++)
    {
        struct win *w = &wins[i];
        int x = w->x, y = w->y < 0 ? sh + w->y : w->y, width = w->w < 0 ? sw : w->w;
        w->hwnd = CreateWindowExA(w->exstyle, "placetest", w->name, WS_POPUP, x, y, width, w->h,
                                  NULL, NULL, inst, NULL);
        ShowWindow(w->hwnd, SW_SHOW);
        UpdateWindow(w->hwnd);
        fprintf(logf, "window %s %p %d,%d %dx%d\n", w->name, w->hwnd, x, y, width, w->h);
    }
    fflush(logf);
    while (GetMessageA(&msg, NULL, 0, 0) > 0)
    {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }
    return 0;
}
