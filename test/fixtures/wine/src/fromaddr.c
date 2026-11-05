/* GetModuleHandleExW(FROM_ADDRESS) on what a native builtin hands out: its
 * static data (the core's: msvcrt's _iob; a module of its own's: msvcp140's
 * std::cout), and its function pointers, which are function-table indices
 * below gex's NATIVE_FN_TOP: a builtin class's window procedure (ntdll's
 * user procs: the core) and a common control's (comctl32: a module of its
 * own). And nothing at an address below the floor that no module holds.
 * Translated builtins pass too: there these are image addresses.
 * gex-nt-arch: x64
 * gex-nt-libs: -lcomctl32
 */
#include <windows.h>
#include <commctrl.h>
#include <stdio.h>
static int failures;
static HMODULE from(const void *p)
{
    HMODULE m = 0;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, p, &m)) return 0;
    return m;
}
static void check(const char *what, const void *p, const char *dll)
{
    HMODULE want = GetModuleHandleA(dll), got = p ? from(p) : 0;
    printf("%s %s: %p -> %p (%s %p)\n", got && got == want ? "ok" : "FAIL", what, p, got, dll, want);
    if (!got || got != want) failures++;
}
static void *wndproc(const WCHAR *cls)
{
    HWND h = CreateWindowExW(0, cls, L"x", 0, 0, 0, 10, 10, HWND_MESSAGE, 0, 0, 0);
    void *p = h ? (void *)GetWindowLongPtrW(h, GWLP_WNDPROC) : NULL;
    if (h) DestroyWindow(h);
    return p;
}
int main(void)
{
    HMODULE msvcrt = LoadLibraryA("msvcrt.dll"), msvcp = LoadLibraryA("msvcp140.dll");
    INITCOMMONCONTROLSEX icc = { sizeof(icc), ICC_LISTVIEW_CLASSES };
    check("msvcrt _iob (core data)", GetProcAddress(msvcrt, "_iob"), "msvcrt.dll");
    if (msvcp) check("msvcp140 std::cout (split data)", GetProcAddress(msvcp, "?cout@std@@3V?$basic_ostream@DU?$char_traits@D@std@@@1@A"), "msvcp140.dll");
    check("STATIC's window procedure (core function)", wndproc(L"STATIC"), "ntdll.dll");
    InitCommonControlsEx(&icc);
    check("SysListView32's window procedure (split function)", wndproc(WC_LISTVIEWW), "comctl32.dll");
    printf("%s nothing at 0x200000: %p\n", from((void *)0x200000) ? "FAIL" : "ok", from((void *)0x200000));
    failures += from((void *)0x200000) != 0;
    printf("%d failures\n", failures);
    return failures != 0;
}
