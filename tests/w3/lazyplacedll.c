/* tests/w3/lazyplace.sh's DLL, built twice (lazyplace_a.dll, never run;
 * lazyplace_b.dll, called). */
#include <windows.h>

__declspec(dllexport) unsigned lazy_mix(unsigned x)
{
    unsigned i;
    for (i = 0; i < 16; i++) x = x * 2654435761u + (x >> 13);
    return x;
}

BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, void *reserved) { return TRUE; }
