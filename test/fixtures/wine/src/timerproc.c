/* Timer procedures, called with a timer procedure's arguments wherever a
 * WM_TIMER is dispatched: Wine's own (a list view's delayed label edit: a
 * click on the selected item's label starts a timer whose procedure,
 * comctl32's, opens the edit box) and the program's, a window's and a
 * thread's, through DispatchMessageW and DispatchMessageA (GetMessage
 * loops), a dialog's modal loop (MessageBox) and win32u's menu loop
 * (TrackPopupMenu), which dispatches in win32u.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -municode
 * gex-nt-libs: -lcomctl32 */
#include <windows.h>
#include <commctrl.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static int app_ticks;
static void CALLBACK app_timer(HWND w, UINT m, UINT_PTR id, DWORD t) { app_ticks++; KillTimer(w, id); }

/* a thread timer's procedure that ends a modal loop on its third call:
 * the message box (its OK) or the menu */
static int modal_ticks;
static HWND modal_box;
static void CALLBACK modal_timer(HWND w, UINT m, UINT_PTR id, DWORD t) {
    if (++modal_ticks != 3) return;
    KillTimer(NULL, id);
    if (modal_box) PostMessageW(modal_box, WM_COMMAND, IDOK, 0);
    else EndMenu();
}
static BOOL CALLBACK find_box(HWND w, LPARAM l) {
    WCHAR cls[16];
    if (GetClassNameW(w, cls, 16) && !lstrcmpW(cls, L"#32770")) { modal_box = w; return FALSE; }
    return TRUE;
}
static void CALLBACK box_timer(HWND w, UINT m, UINT_PTR id, DWORD t) {
    if (!modal_box) EnumThreadWindows(GetCurrentThreadId(), find_box, 0);
    modal_timer(w, m, id, t);
}

/* GetMessage and DispatchMessageA or W until the timer procedure has run n times */
static int get_ticks;
static void CALLBACK get_timer(HWND w, UINT m, UINT_PTR id, DWORD t) {
    if (++get_ticks == 3) { KillTimer(w, id); PostThreadMessageW(GetCurrentThreadId(), WM_APP, 0, 0); }
}
static int get_loop(BOOL ansi) {
    MSG msg;
    DWORD end = GetTickCount() + 5000;
    while ((LONG)(end - GetTickCount()) > 0 && (ansi ? GetMessageA(&msg, NULL, 0, 0) : GetMessageW(&msg, NULL, 0, 0))) {
        if (msg.message == WM_APP && !msg.hwnd) return 1;
        if (ansi) DispatchMessageA(&msg); else DispatchMessageW(&msg);
    }
    return 0;
}

static void pump(DWORD ms) {
    DWORD end = GetTickCount() + ms;
    MSG msg;
    while ((LONG)(end - GetTickCount()) > 0) {
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) { TranslateMessage(&msg); DispatchMessageW(&msg); }
        MsgWaitForMultipleObjects(0, NULL, FALSE, 20, QS_ALLINPUT);
    }
}

int wmain(int argc, WCHAR **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    INITCOMMONCONTROLSEX icc = { sizeof(icc), ICC_LISTVIEW_CLASSES };
    CHECK("InitCommonControlsEx", InitCommonControlsEx(&icc));
    HWND top = CreateWindowExW(0, L"STATIC", L"top", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0, 400, 300, NULL, NULL, NULL, NULL);
    HWND lv = CreateWindowExW(0, WC_LISTVIEWW, L"", WS_CHILD | WS_VISIBLE | LVS_REPORT | LVS_EDITLABELS | LVS_SINGLESEL,
                              0, 0, 380, 280, top, (HMENU)1, NULL, NULL);
    CHECK("a list view", lv != NULL);
    LVCOLUMNW col = { LVCF_WIDTH | LVCF_TEXT, 0, 200, (WCHAR *)L"name" };
    SendMessageW(lv, LVM_INSERTCOLUMNW, 0, (LPARAM)&col);
    LVITEMW it = { LVIF_TEXT, 0, 0, 0, 0, (WCHAR *)L"file.txt" };
    CHECK("an item", SendMessageW(lv, LVM_INSERTITEMW, 0, (LPARAM)&it) == 0);

    /* an application's timer procedure, for comparison */
    SetTimer(top, 7, 10, app_timer);
    pump(300);
    CHECK("the application's timer procedure ran", app_ticks == 1);

    /* a window's and a thread's timer procedures through GetMessage loops */
    get_ticks = 0;
    SetTimer(top, 8, 10, get_timer);
    CHECK("a window's timer procedure, DispatchMessageW", get_loop(FALSE) && get_ticks == 3);
    get_ticks = 0;
    SetTimer(NULL, 0, 10, get_timer);
    CHECK("a thread's timer procedure, DispatchMessageA", get_loop(TRUE) && get_ticks == 3);

    /* modal loops: a message box's (user32's dialog loop) and a menu's (win32u's) */
    modal_ticks = 0;
    SetTimer(NULL, 0, 50, box_timer);
    int box = MessageBoxW(top, L"closes itself", L"timerproc", MB_OK);
    CHECK("a thread's timer procedure in a message box's loop", modal_ticks == 3 && box == IDOK);
    HMENU menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, 1, L"item");
    modal_ticks = 0; modal_box = NULL;
    SetForegroundWindow(top);
    SetTimer(NULL, 0, 50, modal_timer);
    TrackPopupMenu(menu, TPM_RETURNCMD, 10, 10, 0, top, NULL);
    CHECK("a thread's timer procedure in a menu's loop", modal_ticks == 3);
    DestroyMenu(menu);

    SetFocus(lv);
    SendMessageW(lv, WM_SETFOCUS, 0, 0);
    RECT r = { LVIR_LABEL };
    SendMessageW(lv, LVM_GETITEMRECT, 0, (LPARAM)&r);
    LPARAM at = MAKELPARAM(r.left + 4, (r.top + r.bottom) / 2);
    /* the first click selects the item, the second (on its label, with
     * no double click between) starts the delayed edit */
    SendMessageW(lv, WM_LBUTTONDOWN, MK_LBUTTON, at);
    SendMessageW(lv, WM_LBUTTONUP, 0, at);
    pump(GetDoubleClickTime() + 300);
    CHECK("the first click selected the item", SendMessageW(lv, LVM_GETITEMSTATE, 0, LVIS_SELECTED) == LVIS_SELECTED);
    SendMessageW(lv, WM_LBUTTONDOWN, MK_LBUTTON, at);
    SendMessageW(lv, WM_LBUTTONUP, 0, at);
    pump(GetDoubleClickTime() + 500);
    HWND edit = (HWND)SendMessageW(lv, LVM_GETEDITCONTROL, 0, 0);
    CHECK("comctl32's timer procedure opened the label's edit box", edit != NULL);
    if (edit) SendMessageW(lv, LVM_CANCELEDITLABEL, 0, 0);
    DestroyWindow(top);
    printf("%d failures\n", failures);
    return failures;
}
