/* test/wow64/reloadbase: a DLL with a unix side loaded, freed, its old base
 * taken by the program's own memory, and loaded again (at another base):
 * its DllMain's unix call setup must find the new base, not the old.
 * Prints "reloadbase ok". */
#include <windows.h>
#include <stdio.h>
#include <string.h>

int main(void)
{
    HMODULE h = LoadLibraryA("crypt32.dll"), h2, from = NULL;
    MEMORY_BASIC_INFORMATION mbi;
    SIZE_T size;
    void *p;

    if (!h) {
        printf("reloadbase: first load failed (%lu)\n", GetLastError());
        return 1;
    }
    VirtualQuery(h, &mbi, sizeof(mbi));
    size = ((IMAGE_NT_HEADERS *)((BYTE *)h + ((IMAGE_DOS_HEADER *)h)->e_lfanew))->OptionalHeader.SizeOfImage;
    FreeLibrary(h);
    if (GetModuleHandleA("crypt32.dll")) {
        printf("reloadbase: crypt32 still loaded after FreeLibrary\n");
        return 1;
    }
    p = VirtualAlloc(h, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (p != (void *)h) {
        printf("reloadbase: old base %p not free (%p, %lu)\n", h, p, GetLastError());
        return 1;
    }
    memset(p, 0x5a, size);
    h2 = LoadLibraryA("crypt32.dll");
    if (!h2 || h2 == h) {
        printf("reloadbase: second load %p (old %p, %lu)\n", h2, h, GetLastError());
        return 1;
    }
    if (!GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            (LPCSTR)GetProcAddress(h2, "CertOpenStore"), &from) || from != h2) {
        printf("reloadbase: CertOpenStore in %p, not %p\n", from, h2);
        return 1;
    }
    printf("reloadbase ok %p -> %p\n", h, h2);
    return 0;
}
