/* A program importing a function its DLL does not export (advapi32's
 * RegOpenKeyTransactedW, a stub in Wine's .spec): it links and runs; with an
 * argument it calls it, which ends the process as Wine's stubs do. */
#include <windows.h>
#include <stdio.h>

LONG WINAPI RegOpenKeyTransactedW(HKEY, LPCWSTR, DWORD, REGSAM, PHKEY, HANDLE, PVOID);

int main(int argc, char **argv)
{
    HKEY key;
    printf("missimp: start\n");
    fflush(stdout);
    if (argc > 1)
        printf("missimp: returned %ld\n", RegOpenKeyTransactedW(HKEY_CURRENT_USER, L"Software", 0, KEY_READ, &key, NULL, NULL));
    printf("missimp: done\n");
    return 0;
}
