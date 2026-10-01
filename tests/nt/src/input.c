/* Input without a display: synthesized keyboard and mouse input (SendInput,
 * SetCursorPos) arriving as hardware messages, TranslateMessage, the
 * synchronous and asynchronous key state, the US keyboard layout's
 * mappings, TrackMouseEvent's WM_MOUSELEAVE, and cursor objects.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -municode */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static UINT msgs[64];
static WPARAM wps[64];
static LPARAM lps[64];
static SHORT shift_at_char;
static int n;

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    if ((m >= WM_KEYFIRST && m <= WM_KEYLAST) || (m >= WM_MOUSEFIRST && m <= WM_MOUSELAST) || m == WM_MOUSELEAVE) {
        if (n < 64) { msgs[n] = m; wps[n] = wp; lps[n] = lp; n++; }
        if (m == WM_CHAR) shift_at_char = GetKeyState(VK_SHIFT);
    }
    if (m == WM_SETCURSOR || m == WM_ERASEBKGND || m == WM_PAINT) return DefWindowProcW(w, m, wp, lp);
    return DefWindowProcW(w, m, wp, lp);
}

static void pump(void) {
    MSG msg;
    while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

static int find(UINT m) { for (int i = 0; i < n; i++) if (msgs[i] == m) return i; return -1; }

static void key(WORD vk, int up) {
    INPUT in = {0};
    in.type = INPUT_KEYBOARD;
    in.ki.wVk = vk;
    in.ki.wScan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
    in.ki.dwFlags = up ? KEYEVENTF_KEYUP : 0;
    SendInput(1, &in, sizeof(in));
}

int wmain(void) {
    CHECK("MapVirtualKey VK_TO_VSC", MapVirtualKeyW('A', MAPVK_VK_TO_VSC) == 0x1e && MapVirtualKeyW(VK_RETURN, MAPVK_VK_TO_VSC) == 0x1c);
    CHECK("MapVirtualKey VSC_TO_VK", MapVirtualKeyW(0x2a, MAPVK_VSC_TO_VK) == VK_SHIFT && MapVirtualKeyW(0x2a, MAPVK_VSC_TO_VK_EX) == VK_LSHIFT);
    CHECK("MapVirtualKey VK_TO_CHAR", MapVirtualKeyW('Q', MAPVK_VK_TO_CHAR) == 'Q' && MapVirtualKeyW(VK_OEM_MINUS, MAPVK_VK_TO_CHAR) == '-');
    BYTE state[256] = {0};
    WCHAR buf[4];
    CHECK("ToUnicode", ToUnicode('A', 0x1e, state, buf, 4, 0) == 1 && buf[0] == 'a');
    state[VK_SHIFT] = 0x80;
    CHECK("ToUnicode with shift", ToUnicode('1', 0x02, state, buf, 4, 0) == 1 && buf[0] == '!');
    CHECK("VkKeyScan", VkKeyScanW('A') == 0x141 && VkKeyScanW('a') == 'A');
    CHECK("the keyboard layout is US English", LOWORD(GetKeyboardLayout(0)) == 0x0409);

    WNDCLASSW wc = {0};
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gex_input";
    RegisterClassW(&wc);
    HWND w = CreateWindowExW(0, L"gex_input", L"input", WS_POPUP | WS_VISIBLE, 100, 100, 200, 150, 0, 0, wc.hInstance, 0);
    SetForegroundWindow(w);
    SetFocus(w);
    pump();
    n = 0;

    key(VK_SHIFT, 0);
    key('B', 0);
    key('B', 1);
    key(VK_SHIFT, 1);
    pump();
    int down = find(WM_KEYDOWN), ch = find(WM_CHAR);
    CHECK("keys arrive as WM_KEYDOWN", down >= 0 && wps[down] == VK_SHIFT && find(WM_KEYUP) > down);
    CHECK("TranslateMessage makes WM_CHAR", ch >= 0 && wps[ch] == 'B');
    CHECK("the key state follows the messages", (shift_at_char & 0x8000) != 0 && (GetKeyState(VK_SHIFT) & 0x8000) == 0);
    CHECK("lParam has the scan code", down >= 0 && ((lps[down] >> 16) & 0xff) == 0x2a);

    n = 0;
    SetCursorPos(150, 130);
    pump();
    int mv = find(WM_MOUSEMOVE);
    CHECK("SetCursorPos moves the mouse over the window", mv >= 0 && LOWORD(lps[mv]) == 50 && HIWORD(lps[mv]) == 30);
    POINT pt;
    CHECK("GetCursorPos", GetCursorPos(&pt) && pt.x == 150 && pt.y == 130);
    TRACKMOUSEEVENT tme = { sizeof(tme), TME_LEAVE, w, 0 };
    CHECK("TrackMouseEvent", TrackMouseEvent(&tme));

    n = 0;
    INPUT in[2] = {0};
    in[0].type = INPUT_MOUSE;
    in[0].mi.dwFlags = MOUSEEVENTF_LEFTDOWN;
    in[1].type = INPUT_MOUSE;
    in[1].mi.dwFlags = MOUSEEVENTF_LEFTUP;
    CHECK("SendInput returns the count", SendInput(2, in, sizeof(INPUT)) == 2);
    pump();
    int ld = find(WM_LBUTTONDOWN), lu = find(WM_LBUTTONUP);
    CHECK("a click is WM_LBUTTONDOWN then UP", ld >= 0 && lu > ld && LOWORD(lps[ld]) == 50 && (wps[ld] & MK_LBUTTON));

    n = 0;
    SetCursorPos(10, 10);
    /* Wine notices the leave on a timer; gex at once. */
    for (int i = 0; i < 50 && find(WM_MOUSELEAVE) < 0; i++) { pump(); Sleep(20); }
    CHECK("leaving the window is WM_MOUSELEAVE", find(WM_MOUSELEAVE) >= 0);

    HCURSOR arrow = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    CHECK("LoadCursor gives a cursor", arrow != 0);
    CHECK("loading it again gives the same one", LoadCursorW(0, (LPCWSTR)IDC_ARROW) == arrow);
    SetCursor(arrow);
    CHECK("SetCursor/GetCursor", GetCursor() == arrow);

    DestroyWindow(w);
    printf("%d failures\n", failures);
    return failures;
}
