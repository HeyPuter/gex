/* test/wow64/overmap's DLL: its code is a run of nops that ends in
 * "return 42", long enough to cover a small builtin's exports when it is
 * mapped where that builtin was. A call anywhere in it returns 42. Popped
 * as a stdcall of two arguments on i386. */
#include <windows.h>

__asm__(".section .text\n"
        ".fill 0x1800, 1, 0x90\n"
        "movl $42, %eax\n"
#ifdef _WIN64
        "ret\n"
#else
        "ret $8\n"
#endif
);

BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, void *reserved)
{
    return TRUE;
}
