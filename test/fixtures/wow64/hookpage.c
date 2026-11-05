/* Detours-style hooks on some of user32's exports, then calls to untouched
 * exports on the same code page (test/wow64/hookpage). IE8 hooks
 * MessageBox and DialogBox this way at start-up: every target made
 * PAGE_EXECUTE_READWRITE, a 5-byte jmp written at each, each given back the
 * protection it reported, in reverse order. 32-bit Wine's user32 is a skeleton whose
 * exports are 4-byte `ud2` stubs bound to its wasm; the writes retire the
 * page's translations, and the stubs beside the hooks must stay bound: a
 * caller translated after the hooks (combase's `call [iat]` in IE8) must
 * reach RegisterClassW, not its stub (c000001d).
 *
 * Prints "hookpage ok" when every call after the hooks works. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

/* First run after the hooks, so tier-1 translates it then, following its
 * import calls into user32. */
static __attribute__((noinline)) int after(HWND d)
{
    WNDCLASSW wc;
    if (GetPropW(d, L"gex-hookpage")) { printf("hookpage: GetPropW after the hooks\n"); return 0; }
    GetQueueStatus(QS_ALLINPUT);
    memset(&wc, 0, sizeof wc);
    wc.lpfnWndProc = DefWindowProcW;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"gex-hookpage";
    if (!RegisterClassW(&wc)) { printf("hookpage: RegisterClassW after the hooks: %lu\n", GetLastError()); return 0; }
    return 1;
}

static HANDLE WINAPI hook(HWND w, LPCSTR s) { (void)w; (void)s; return (HANDLE)0x1234; }

int main(void)
{
    static const char *names[] = { "MessageBoxExW", "MessageBoxExA", "DialogBoxParamW", "DialogBoxParamA", "GetPropA" };
    enum { N = sizeof names / sizeof names[0] };
    HMODULE u = GetModuleHandleA("user32.dll");
    BYTE *t[N];
    DWORD was[N], old;
    HWND d = GetDesktopWindow();
    int (*volatile fp)(HWND) = after;
    int i, same_page = 0;

    for (i = 0; i < N; i++) {
        t[i] = (BYTE *)GetProcAddress(u, names[i]);
        if (!t[i]) { printf("hookpage: no %s\n", names[i]); return 1; }
        same_page += ((UINT_PTR)t[i] >> 12) == ((UINT_PTR)GetProcAddress(u, "RegisterClassW") >> 12);
    }
    for (i = 0; i < N; i++)
        if (!VirtualProtect(t[i], 5, PAGE_EXECUTE_READWRITE, &was[i])) { printf("hookpage: VirtualProtect: %lu\n", GetLastError()); return 1; }
    for (i = 0; i < N; i++) {
        t[i][0] = 0xe9;
        *(INT32 *)(t[i] + 1) = (INT32)((BYTE *)hook - (t[i] + 5));
    }
    for (i = N - 1; i >= 0; i--)
        VirtualProtect(t[i], 5, was[i], &old);
    FlushInstructionCache(GetCurrentProcess(), NULL, 0);
    /* Through a pointer, so not translated before now. */
    if (!fp(d)) return 1;
    /* How many hooks shared RegisterClassW's page (0 would test nothing). */
    printf("hookpage ok %d\n", same_page);
    return 0;
}
