/* reexport: the guest of test/wow64/reexport. imagehlp's SymSetOptions is
 * dbghelp's (a `-import` re-export of a DLL imagehlp only delay-imports),
 * called through the address GetProcAddress gives.
 *
 *   reexport      says "reexport 10" */
#include <windows.h>
#include <stdio.h>

int main(void) {
    HMODULE h = LoadLibraryA("imagehlp.dll");
    DWORD (WINAPI *set)(DWORD) = h ? (void *)GetProcAddress(h, "SymSetOptions") : NULL;
    if (!set) { printf("reexport: no SymSetOptions\n"); return 1; }
    printf("reexport %lx\n", set(0x10));
    return 0;
}
