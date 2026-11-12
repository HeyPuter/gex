/* Escape closes the desktop's menus while a program had the keyboard
 * (test/wine64/menuesc): on Windows, opening the Start menu or the desktop's
 * context menu makes the menu's thread foreground, so the menu gets the
 * keyboard and Escape closes it. The program's window W (333x222 at 300,200)
 * takes the foreground; the script (menuesc.wl) opens the Start menu and
 * presses Escape, clicks W, then right-clicks the desktop and presses Escape.
 * For each menu (a visible "#32768" window, in explorer's process) this
 * checks:
 *   - W was foreground before it opened;
 *   - while it is up, the foreground window is of the menu's thread;
 *   - it goes away, and no Escape reached W while it was up (one that did
 *     leaves the menu up until the script's next click closes it).
 * Prints "ok"/"FAIL" lines and "N failures"; gives up after SECONDS
 * (default 120).
 *
 *   menuesc.exe [SECONDS]
 * gex-nt-arch: x64
 * gex-nt-libs: -lgdi32
 */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

/* Each line in one write, as deco.c's. */
#define printf(...) do { char line_[256]; snprintf(line_, sizeof(line_), __VA_ARGS__); fputs(line_, stdout); fflush(stdout); } while (0)

static HWND w;
static int escapes;

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    if (msg == WM_KEYDOWN && wp == VK_ESCAPE) escapes++;
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

static HWND menu_up(void)
{
    HWND m = FindWindowW(L"#32768", NULL);
    return m && IsWindowVisible(m) ? m : 0;
}

int main(int argc, char **argv)
{
    WNDCLASSW wc = {0};
    DWORD secs = argc > 1 ? atoi(argv[1]) : 120, start, menu_tid = 0;
    int failures = 0, menus = 0, up = 0, fg_ok = 0, w_fg = 0, esc_at_open = 0;
    const char *names[] = { "the Start menu", "the desktop menu" };
    MSG msg;
    int i;

    /* the default desktop, "shell" (HKCU\Software\Wine\Explorer), comes up
     * with this program's first window: wait for its taskbar */
    CreateWindowExW(0, L"STATIC", L"", WS_POPUP, 0, 0, 1, 1, 0, 0, 0, 0);
    for (i = 0; i < 600 && !FindWindowW(L"Shell_TrayWnd", NULL); i++) Sleep(50);
    printf("menuesc: taskbar %s\n", FindWindowW(L"Shell_TrayWnd", NULL) ? "up" : "not up");

    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    wc.lpszClassName = L"menuesc";
    RegisterClassW(&wc);
    w = CreateWindowExW(0, L"menuesc", L"menuesc", WS_POPUP | WS_VISIBLE, 300, 200, 333, 222, 0, 0, 0, 0);
    SetForegroundWindow(w);
    SetFocus(w);
    printf("menuesc: window up, foreground %s\n", GetForegroundWindow() == w ? "W" : "not W");
    start = GetTickCount();
    while (menus < 2 && GetTickCount() - start < secs * 1000)
    {
        HWND m, fg;
        while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) { TranslateMessage(&msg); DispatchMessageW(&msg); }
        m = menu_up();
        fg = GetForegroundWindow();
        if (!m && !up && fg == w) w_fg = 1;
        if (m && !up)
        {
            if (w_fg) printf("ok W was foreground before %s opened\n", names[menus]);
            else { printf("FAIL W was not foreground before %s opened\n", names[menus]); failures++; }
            w_fg = 0;
            up = 1;
            menu_tid = GetWindowThreadProcessId(m, NULL);
            esc_at_open = escapes;
            fg_ok = 0;
            printf("menuesc: %s up at %lu ms\n", names[menus], GetTickCount() - start);
        }
        if (m && up && fg && GetWindowThreadProcessId(fg, NULL) == menu_tid) fg_ok = 1;
        if (!m && up)
        {
            const char *n = names[menus];
            up = 0;
            printf("menuesc: %s gone at %lu ms\n", n, GetTickCount() - start);
            if (fg_ok) printf("ok %s's thread was foreground while it was up\n", n);
            else { printf("FAIL %s's thread was never foreground while it was up\n", n); failures++; }
            if (escapes == esc_at_open) printf("ok no Escape reached W while %s was up\n", n);
            else { printf("FAIL %d Escape(s) reached W while %s was up\n", escapes - esc_at_open, n); failures++; }
            menus++;
        }
        MsgWaitForMultipleObjects(0, NULL, FALSE, 50, QS_ALLINPUT);
    }
    if (menus < 2)
    {
        printf("FAIL %s %s within %lu s\n", names[menus], up ? "still up" : "never up", secs);
        failures++;
    }
    printf("%d failures\n", failures);
    DestroyWindow(w);
    return failures;
}
