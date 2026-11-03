/* Timer procedures in Wine's own DLLs: a list view's delayed label edit (a
 * click on the selected item's label starts a timer whose procedure,
 * comctl32's, opens the edit box), dispatched by DispatchMessageW, which
 * calls a timer procedure with the arguments of a window procedure.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -municode
 * gex-nt-libs: -lcomctl32 */
#include <windows.h>
#include <commctrl.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static int app_ticks;
static void CALLBACK app_timer(HWND w, UINT m, UINT_PTR id, DWORD t) { app_ticks++; KillTimer(w, id); }

static void pump(DWORD ms) {
    DWORD end = GetTickCount() + ms;
    MSG msg;
    while ((LONG)(end - GetTickCount()) > 0) {
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) { TranslateMessage(&msg); DispatchMessageW(&msg); }
        MsgWaitForMultipleObjects(0, NULL, FALSE, 20, QS_ALLINPUT);
    }
}

int wmain(void) {
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
