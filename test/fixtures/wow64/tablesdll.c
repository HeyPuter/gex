/* tables.c's DLL: no unit, so its DllMain and tables_call run on
 * tier-1. */
#include <windows.h>

typedef int (*cb_t)(int);

BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, LPVOID reserved)
{
    (void)h; (void)reason; (void)reserved;
    return TRUE;
}

/* The sum of f(i) for i below n: n * n for f(x) = 2x + 1. */
__declspec(dllexport) int __stdcall tables_call(cb_t f, int n)
{
    int s = 0, i;
    for (i = 0; i < n; i++)
        s += f(i);
    return s;
}
