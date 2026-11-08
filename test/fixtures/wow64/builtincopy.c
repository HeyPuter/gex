/* test/wow64/builtincopy: a builtin DLL already loaded, loaded again from
 * a copy of it at another path (a program that ships its own ucrtbase.dll
 * and loads it by its full path, as Zoom's installer does): Wine hands back
 * the loaded DLL, and the DLL keeps its state. ucrtbase's file mode, set
 * before, is still set after, and its heap still allocates. Prints
 * "builtincopy ok". */
#include <windows.h>
#include <stdio.h>

int main(void)
{
    char sys[MAX_PATH], copy[MAX_PATH];
    HMODULE u = LoadLibraryA("ucrtbase.dll"), again;
    int (__cdecl *set_fmode)(int) = u ? (void *)GetProcAddress(u, "_set_fmode") : NULL;
    int (__cdecl *get_fmode)(int *) = u ? (void *)GetProcAddress(u, "_get_fmode") : NULL;
    void *(__cdecl *ucalloc)(size_t, size_t) = u ? (void *)GetProcAddress(u, "calloc") : NULL;
    void (__cdecl *ufree)(void *) = u ? (void *)GetProcAddress(u, "free") : NULL;
    int mode = 0;
    void *p;

    setvbuf(stdout, NULL, _IONBF, 0);
    if (!set_fmode || !get_fmode || !ucalloc || !ufree) { printf("builtincopy: no ucrtbase (%p)\n", u); return 1; }
    set_fmode(0x8000 /* _O_BINARY */);
    GetModuleFileNameA(u, sys, sizeof(sys));
    GetTempPathA(sizeof(copy), copy);
    lstrcatA(copy, "ucrtbase.dll");
    if (!CopyFileA(sys, copy, FALSE)) { printf("builtincopy: copying %s to %s failed (%lu)\n", sys, copy, GetLastError()); return 1; }
    again = LoadLibraryExA(copy, NULL, 0);
    printf("builtincopy: %s at %p, its copy %s loaded as %p\n", sys, u, copy, again);
    p = ucalloc(1, 64);
    get_fmode(&mode);
    printf("builtincopy: after the load, calloc %p, file mode %#x\n", p, mode);
    if (!p || mode != 0x8000) return 1;
    ufree(p);
    if (again) FreeLibrary(again);
    p = ucalloc(1, 64);
    get_fmode(&mode);
    printf("builtincopy: after its free, calloc %p, file mode %#x\n", p, mode);
    if (!p || mode != 0x8000) return 1;
    ufree(p);
    DeleteFileA(copy);
    printf("builtincopy ok\n");
    return 0;
}
