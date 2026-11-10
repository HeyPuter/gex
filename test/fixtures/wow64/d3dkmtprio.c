/* gdi32's D3DKMTSetProcessSchedulingPriorityClass, found by name as a
 * program that checks for it does: GetProcAddress, then a call for this
 * process at above normal and back at normal. A missing export is a NULL
 * here; a missing import elsewhere is a call to a bad address.
 * Prints "d3dkmtprio ok" or what failed. */
#include <windows.h>
#include <stdio.h>

typedef LONG (WINAPI *SETPRIO_FN)(HANDLE, int);

int main(void)
{
    HMODULE gdi = LoadLibraryA("gdi32.dll");
    SETPRIO_FN set = gdi ? (SETPRIO_FN)GetProcAddress(gdi, "D3DKMTSetProcessSchedulingPriorityClass") : NULL;
    LONG a, b;

    if (!set) { printf("d3dkmtprio: no export (gdi32 %p)\n", gdi); return 1; }
    a = set(GetCurrentProcess(), 3 /* ABOVE_NORMAL */);
    b = set(GetCurrentProcess(), 2 /* NORMAL */);
    if (a || b) { printf("d3dkmtprio: status %#lx, %#lx\n", (unsigned long)a, (unsigned long)b); return 1; }
    printf("d3dkmtprio ok\n");
    return 0;
}
