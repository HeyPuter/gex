/* msvcp60 loaded: its DllMain makes the standard streams, whose locale
 * calls string destructors its sources declare apart (msvcp60's own
 * returning nothing, the msvcp90 header's a pointer). A direct call whose
 * type is not the function's is a trap in wasm, which ended Notepad2.
 *
 * Prints "msvcp60 ok" and exits 0. */
#include <windows.h>
#include <stdio.h>

int main(void)
{
    HMODULE h = LoadLibraryA("msvcp60.dll");

    if (!h)
    {
        printf("LoadLibrary msvcp60 failed: %lu\n", GetLastError());
        return 1;
    }
    if (!GetProcAddress(h, "?_Init@locale@std@@CAPAV_Locimp@12@XZ") &&
        !GetProcAddress(h, "?_Init@locale@std@@CAPEAV_Locimp@12@XZ"))
    {
        printf("no locale::_Init\n");
        return 1;
    }
    FreeLibrary(h);
    printf("msvcp60 ok\n");
    return 0;
}
