/* lresult.exe: negative LRESULTs reach a 64-bit caller sign-extended, through
 * SendMessageW, SendDlgItemMessageW and CallWindowProcW (an empty listbox's
 * LB_GETCURSEL is LB_ERR), at top level, with a WH_CALLWNDPROC hook in the
 * thread, and from a dialog procedure (a message sent inside a user
 * callback), and a pointer LB_SETITEMDATA stored comes back whole. Built with
 * no -luser32, as MinGW programs are (Notepad++): its user32 calls must be
 * user32's, not another DLL's that exports the names too (unicows, whose
 * adapters took 32-bit words). Prints "lresult FAIL ..." per failure, then
 * "ok lresult" and "0 failures" when all pass. */
#include <windows.h>
#include <stdio.h>
static int failures;
static void check(const char *what, LONG_PTR got, LONG_PTR want)
{
    if (got != want) { printf("lresult FAIL %s: %llx, want %llx\n", what, (unsigned long long)got, (unsigned long long)want); failures++; }
}
static LRESULT WINAPI neg_proc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_USER) return -2;
    if (m == WM_USER + 1) return (LRESULT)0x123456789abcdefull;
    if (m == WM_USER + 2)   /* nested sends, wp deep: each level's result is -2 - its depth */
    {
        LRESULT r = w ? SendMessageW(h, WM_USER + 2, w - 1, 0) : -2;
        if (r != -2 - (LRESULT)(w ? w - 1 : 0) && w) { printf("lresult FAIL nested depth %u: %llx\n", (unsigned)w, (unsigned long long)r); failures++; }
        return -2 - (LRESULT)w;
    }
    return DefWindowProcW(h, m, w, l);
}
static HWND own;
static void sends(const char *where, HWND dlg, HWND lb)
{
    char n[96];
#define C(what, got, want) (snprintf(n, sizeof(n), "%s %s", where, what), check(n, got, want))
    C("SendMessageW LB_GETCURSEL", SendMessageW(lb, LB_GETCURSEL, 0, 0), LB_ERR);
    C("SendDlgItemMessageW LB_GETCURSEL", SendDlgItemMessageW(dlg, 77, LB_GETCURSEL, 0, 0), LB_ERR);
    C("SendDlgItemMessageW LB_GETITEMDATA", SendDlgItemMessageW(dlg, 77, LB_GETITEMDATA, 5, 0), LB_ERR);
    C("SendMessageW own -2", SendMessageW(own, WM_USER, 0, 0), -2);
    C("SendMessageW own 64-bit", SendMessageW(own, WM_USER + 1, 0, 0), (LRESULT)0x123456789abcdefull);
    C("CallWindowProcW own -2", CallWindowProcW(neg_proc, own, WM_USER, 0, 0), -2);
    C("SendMessageA own -2", SendMessageA(own, WM_USER, 0, 0), -2);
    C("SendMessageW own nested 12", SendMessageW(own, WM_USER + 2, 12, 0), -14);
#undef C
}
static int in_dialog, hooked;
static LRESULT CALLBACK cwp_hook(int code, WPARAM w, LPARAM l) { hooked++; return CallNextHookEx(NULL, code, w, l); }
static INT_PTR WINAPI dlg_proc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_INITDIALOG)
    {
        CreateWindowW(L"listbox", L"", WS_CHILD, 0, 0, 50, 50, h, (HMENU)77, NULL, NULL);
        CreateWindowW(L"edit", L"", WS_CHILD, 0, 60, 50, 20, h, (HMENU)78, NULL, NULL);
        SetDlgItemInt(h, 78, 4, FALSE);   /* EN_CHANGE, sent to us from inside this message */
        PostMessageW(h, WM_USER + 5, 0, 0);
        return TRUE;
    }
    if (m == WM_COMMAND && LOWORD(w) == 78 && HIWORD(w) == EN_CHANGE)
    {
        in_dialog++;
        sends("dialog EN_CHANGE", h, GetDlgItem(h, 77));
        return TRUE;
    }
    if (m == WM_USER + 5) { sends("dialog", h, GetDlgItem(h, 77)); EndDialog(h, 1); return TRUE; }
    return FALSE;
}
int main(int argc, char **argv)
{
    HWND dlg, lb;
    WNDCLASSW wc = {0};
    static struct { DLGTEMPLATE t; WORD menu, cls, title; } tpl;
    wc.lpfnWndProc = neg_proc; wc.hInstance = GetModuleHandleW(NULL); wc.lpszClassName = L"lresult";
    RegisterClassW(&wc);
    own = CreateWindowW(L"lresult", L"", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, NULL, NULL, wc.hInstance, NULL);
    dlg = CreateWindowW(L"static", L"", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, NULL, NULL, NULL, NULL);
    lb = CreateWindowW(L"listbox", L"", WS_CHILD, 0, 0, 50, 50, dlg, (HMENU)77, NULL, NULL);
    sends("top", dlg, lb);
    check("SendMessageW LB_ADDSTRING", SendMessageW(lb, LB_ADDSTRING, 0, (LPARAM)L"a"), 0);
    check("SendMessageW LB_SETITEMDATA", SendMessageW(lb, LB_SETITEMDATA, 0, (LPARAM)&failures) != LB_ERR, 1);
    check("SendMessageW LB_GETITEMDATA", SendMessageW(lb, LB_GETITEMDATA, 0, 0), (LONG_PTR)&failures);
    /* again with a WH_CALLWNDPROC hook in the thread (Notepad++ has one) */
    {
        HHOOK hk = SetWindowsHookExW(WH_CALLWNDPROC, cwp_hook, NULL, GetCurrentThreadId());
        sends("hooked", dlg, lb);
        check("hook installed", hk != NULL, 1);
        check("hook called", hooked > 0, 1);
    }
    tpl.t.style = WS_POPUP | DS_MODALFRAME; tpl.t.cx = 100; tpl.t.cy = 100;
    check("DialogBoxIndirectParamW", DialogBoxIndirectParamW(NULL, &tpl.t, NULL, dlg_proc, 0), 1);
    check("dialog EN_CHANGE seen", in_dialog, 1);
    if (!failures) printf("ok lresult\n");
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
