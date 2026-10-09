/* A loaded DLL mapped again as an image (an image section of its file,
 * as wineboot's install maps ntdll): the second mapping
 * must not disturb the loaded one. Under native Wine (W3) gex used to
 * write the loaded DLL's variables again at each mapping, which reset
 * ntdll's Unix-call handle, and ntdll's next Unix call (its debug output
 * here) went through a table at address 1.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
typedef int (__cdecl *dbg_output_t)(const char *);
int main(void) {
    HMODULE nt = GetModuleHandleA("ntdll.dll");
    char path[MAX_PATH];
    HANDLE file, section;
    void *view;
    dbg_output_t out = (dbg_output_t)GetProcAddress(nt, "__wine_dbg_output");
    int k;
    if (!out) { printf("FAIL no __wine_dbg_output\n"); return 1; }
    out("imageview: a line of debug output\n");
    for (k = 0; k < 2; k++) {
        GetModuleFileNameA(nt, path, sizeof path);
        file = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING, 0, 0);
        if (file == INVALID_HANDLE_VALUE) { printf("FAIL open %s %lu\n", path, GetLastError()); return 1; }
        section = CreateFileMappingA(file, 0, PAGE_READONLY | SEC_IMAGE, 0, 0, 0);
        if (!section) { printf("FAIL image section %lu\n", GetLastError()); return 1; }
        view = MapViewOfFile(section, FILE_MAP_READ, 0, 0, 0);
        if (!view) { printf("FAIL map %lu\n", GetLastError()); return 1; }
        if (view == (void *)nt) { printf("FAIL the view is the loaded ntdll\n"); return 1; }
        /* a Unix call of ntdll's, after the view */
        out("imageview: a line of debug output\n");
        UnmapViewOfFile(view);
        CloseHandle(section);
        CloseHandle(file);
        out("imageview: a line of debug output\n");
    }
    printf("ok ntdll mapped twice as an image view\n");
    return 0;
}
