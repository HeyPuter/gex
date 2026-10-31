/* USER without a display: classes, window creation and its messages, window
 * data and properties, sending and posting, timers, the quit message, the
 * window tree, moving and sizing, destruction, registered messages, a
 * message sent from another thread, SetWindowPos from another thread, and
 * MsgWaitForMultipleObjects.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -municode */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static UINT log_msgs[256];
static int nlog;
static int seen(UINT m) { for (int i = 0; i < nlog; i++) if (log_msgs[i] == m) return i + 1; return 0; }
static void *create_param;
static WCHAR create_name[64];
static int destroyed, nc_destroyed, timer_ticks, user_msgs, moved_x, sized_w;
static LPARAM last_lparam;
static DWORD poschanging_tid;

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    if (nlog < 256) log_msgs[nlog++] = m;
    switch (m) {
    case WM_NCCREATE: {
        CREATESTRUCTW *cs = (CREATESTRUCTW *)lp;
        create_param = cs->lpCreateParams;
        if (cs->lpszName) lstrcpynW(create_name, cs->lpszName, 64);
        break;
    }
    case WM_CREATE: return 0;
    case WM_USER + 7: return wp * 3 + lp;
    case WM_USER + 1: user_msgs++; last_lparam = lp; return 0;
    case WM_TIMER: timer_ticks++; return 0;
    case WM_WINDOWPOSCHANGING: poschanging_tid = GetCurrentThreadId(); break;
    case WM_MOVE: moved_x = (short)LOWORD(lp); return 0;
    case WM_SIZE: sized_w = LOWORD(lp); return 0;
    case WM_DESTROY: destroyed++; return 0;
    case WM_NCDESTROY: nc_destroyed++; break;
    }
    return DefWindowProcW(w, m, wp, lp);
}

static HWND target;
static DWORD WINAPI sender(void *p) {
    Sleep(10);
    return (DWORD)SendMessageW(target, WM_USER + 7, 5, 1);
}

/* SetWindowPos on another thread's window: the owner moves it (Wine's
 * WM_WINE_SETWINDOWPOS), so its procedure sees WM_WINDOWPOSCHANGING on the
 * owner's thread. */
static DWORD owner_tid;
static DWORD WINAPI mover(void *p) {
    Sleep(10);
    return (DWORD)SetWindowPos(target, 0, 70, 80, 320, 220, SWP_NOZORDER | SWP_NOACTIVATE);
}

static BOOL CALLBACK count_child(HWND h, LPARAM lp) { (*(int *)lp)++; return TRUE; }

int wmain(void) {
    HINSTANCE hi = GetModuleHandleW(0);
    WNDCLASSEXW wc = {sizeof wc};
    wc.lpfnWndProc = proc; wc.hInstance = hi; wc.lpszClassName = L"gextest";
    wc.cbWndExtra = 16; wc.cbClsExtra = 8; wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    ATOM atom = RegisterClassExW(&wc);
    CHECK("RegisterClassEx", atom >= 0xc000);
    WNDCLASSEXW got = {sizeof got};
    CHECK("GetClassInfoEx", GetClassInfoExW(hi, L"gextest", &got) && got.cbWndExtra == 16 && got.lpfnWndProc == proc);
    CHECK("class registered twice fails", !RegisterClassExW(&wc) && GetLastError() == ERROR_CLASS_ALREADY_EXISTS);

    HWND w = CreateWindowExW(0, L"gextest", L"gex window", WS_OVERLAPPEDWINDOW, 100, 120, 640, 480, 0, 0, hi, (void *)0x1234);
    CHECK("CreateWindowEx", w != 0 && IsWindow(w));
    CHECK("creation messages", seen(WM_NCCREATE) && seen(WM_CREATE) && seen(WM_NCCALCSIZE) && seen(WM_NCCREATE) < seen(WM_CREATE));
    CHECK("create params and name", create_param == (void *)0x1234 && lstrcmpW(create_name, L"gex window") == 0);
    WCHAR text[64];
    CHECK("GetWindowText", GetWindowTextW(w, text, 64) == 10 && lstrcmpW(text, L"gex window") == 0);
    RECT r, cr;
    GetWindowRect(w, &r); GetClientRect(w, &cr);
    CHECK("window rect", r.left == 100 && r.top == 120 && r.right == 740 && r.bottom == 600);
    RECT adj = {0, 0, cr.right, cr.bottom};
    AdjustWindowRectEx(&adj, WS_OVERLAPPEDWINDOW, FALSE, 0);
    CHECK("client rect matches AdjustWindowRect", cr.left == 0 && cr.top == 0 && adj.right - adj.left == 640 && adj.bottom - adj.top == 480);

    SetWindowLongPtrW(w, GWLP_USERDATA, 0x5555);
    SetWindowLongW(w, 8, 0x77);
    CHECK("window longs", GetWindowLongPtrW(w, GWLP_USERDATA) == 0x5555 && GetWindowLongW(w, 8) == 0x77 &&
          (GetWindowLongW(w, GWL_STYLE) & WS_OVERLAPPEDWINDOW) == WS_OVERLAPPEDWINDOW);
    CHECK("wndproc long", (WNDPROC)GetWindowLongPtrW(w, GWLP_WNDPROC) == proc);
    SetPropW(w, L"gexprop", (HANDLE)0x42);
    CHECK("properties", GetPropW(w, L"gexprop") == (HANDLE)0x42 && RemovePropW(w, L"gexprop") == (HANDLE)0x42 && !GetPropW(w, L"gexprop"));

    CHECK("SendMessage", SendMessageW(w, WM_USER + 7, 4, 2) == 14);
    SendMessageW(w, WM_SETTEXT, 0, (LPARAM)L"renamed");
    CHECK("WM_SETTEXT", GetWindowTextW(w, text, 64) == 7 && lstrcmpW(text, L"renamed") == 0);

    PostMessageW(w, WM_USER + 1, 0, 99);
    MSG msg;
    int got_posted = 0;
    while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) { if (msg.message == WM_USER + 1) got_posted++; DispatchMessageW(&msg); }
    CHECK("PostMessage and PeekMessage", got_posted == 1 && user_msgs == 1 && last_lparam == 99);
    CHECK("queue empty", !PeekMessageW(&msg, 0, 0, 0, PM_NOREMOVE));

    SetTimer(w, 1, 10, 0);
    DWORD t0 = GetTickCount();
    while (timer_ticks < 3 && GetTickCount() - t0 < 3000 && GetMessageW(&msg, 0, 0, 0)) DispatchMessageW(&msg);
    KillTimer(w, 1);
    CHECK("timer", timer_ticks >= 3);

    CHECK("not visible yet", !IsWindowVisible(w));
    ShowWindow(w, SW_SHOW);
    CHECK("ShowWindow", IsWindowVisible(w) && seen(WM_SHOWWINDOW));
    nlog = 0;
    SetWindowPos(w, 0, 50, 60, 300, 200, SWP_NOZORDER | SWP_NOACTIVATE);
    GetWindowRect(w, &r);
    CHECK("SetWindowPos", r.left == 50 && r.top == 60 && r.right == 350 && r.bottom == 260);
    CHECK("move and size messages", seen(WM_WINDOWPOSCHANGING) && seen(WM_WINDOWPOSCHANGED) && moved_x > 50 && sized_w > 0 && sized_w < 300);

    HWND child = CreateWindowExW(0, L"gextest", L"child", WS_CHILD | WS_VISIBLE, 5, 5, 50, 50, w, (HMENU)7, hi, 0);
    CHECK("child window", child && GetParent(child) == w && IsChild(w, child) && GetWindow(w, GW_CHILD) == child && GetDlgCtrlID(child) == 7);
    int nchild = 0;
    EnumChildWindows(w, count_child, (LPARAM)&nchild);
    CHECK("EnumChildWindows", nchild == 1);
    CHECK("FindWindow", FindWindowW(L"gextest", L"renamed") == w);

    UINT rm1 = RegisterWindowMessageW(L"gex-message"), rm2 = RegisterWindowMessageW(L"GEX-MESSAGE");
    CHECK("RegisterWindowMessage", rm1 >= 0xc000 && rm1 == rm2);

    target = w;
    HANDLE th = CreateThread(0, 0, sender, 0, 0, 0);
    DWORD code = 0;
    while (WaitForSingleObject(th, 0) == WAIT_TIMEOUT)
        if (MsgWaitForMultipleObjects(1, &th, FALSE, 1000, QS_ALLINPUT) == WAIT_OBJECT_0 + 1)
            while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
    GetExitCodeThread(th, &code);
    CHECK("SendMessage from another thread", code == 16);

    owner_tid = GetCurrentThreadId();
    poschanging_tid = 0;
    th = CreateThread(0, 0, mover, 0, 0, 0);
    while (WaitForSingleObject(th, 0) == WAIT_TIMEOUT)
        if (MsgWaitForMultipleObjects(1, &th, FALSE, 1000, QS_ALLINPUT) == WAIT_OBJECT_0 + 1)
            while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
    GetExitCodeThread(th, &code);
    GetWindowRect(w, &r);
    CHECK("SetWindowPos from another thread", code && r.left == 70 && r.top == 80 && r.right == 390 && r.bottom == 300);
    CHECK("it runs on the owner's thread", poschanging_tid == owner_tid);

    HANDLE ev = CreateEventW(0, TRUE, FALSE, 0);
    PostThreadMessageW(GetCurrentThreadId(), WM_USER + 3, 0, 0);
    CHECK("MsgWait sees a posted message", MsgWaitForMultipleObjects(1, &ev, FALSE, 1000, QS_POSTMESSAGE) == WAIT_OBJECT_0 + 1);
    SetEvent(ev);
    CHECK("MsgWait sees a handle", MsgWaitForMultipleObjects(1, &ev, FALSE, 1000, QS_POSTMESSAGE) == WAIT_OBJECT_0);
    CHECK("thread message", PeekMessageW(&msg, 0, 0, 0, PM_REMOVE) && msg.message == WM_USER + 3 && msg.hwnd == 0);

    DestroyWindow(w);
    CHECK("DestroyWindow", !IsWindow(w) && !IsWindow(child) && destroyed == 2 && nc_destroyed == 2);
    PostQuitMessage(5);
    CHECK("quit message", GetMessageW(&msg, 0, 0, 0) == 0 && msg.wParam == 5);
    CHECK("UnregisterClass", UnregisterClassW(L"gextest", hi));
    printf("%d failures\n", failures);
    return failures;
}
