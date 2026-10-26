/* A window made in a DllMain, the loader lock held, before the process has
 * a desktop: under native Wine on gex (32-bit Wine) the desktop is a thread of the
 * process, which needed the lock its creator held: explorer is a process
 * of its own again (docs/wine-native.md).
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
int main(void) {
    HMODULE m = LoadLibraryA("attachwin_dll.dll");
    HWND *h = m ? (HWND *)GetProcAddress(m, "attachwin_hwnd") : 0;
    int made = h && *h && IsWindow(*h);
    printf("%s the DLL loads\n", m ? "ok" : "FAIL");
    printf("%s its DllMain made a window\n", made ? "ok" : "FAIL");
    printf("%d failures\n", !m + !made);
    return !made;
}
