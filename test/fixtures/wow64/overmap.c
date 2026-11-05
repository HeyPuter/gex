/* test/wow64/overmap: an ordinary PE image mapped where a builtin was. A
 * builtin (advpack, a native module's skeleton) is loaded, one of its
 * exports called, and it is freed. overdll.dll, copied with its ImageBase
 * set to the builtin's base, is then mapped as an image there (Wine maps a
 * DLL without dynamic base at its ImageBase), and the same address is
 * called again. It is overdll's code now, whose nops end in "return 42";
 * the builtin's export, still bound there, returned its own answer.
 * Prints "overmap ok". */
#include <windows.h>
#include <stdio.h>

typedef BOOL (WINAPI *admin_fn)(DWORD, DWORD *);

int main(void)
{
    char tmp[MAX_PATH];
    HMODULE h = LoadLibraryA("advpack.dll");
    admin_fn f;
    HANDLE file, map;
    void *view;
    DWORD r, n;
    static unsigned char buf[1 << 20];
    IMAGE_NT_HEADERS *nt;

    if (!h || !(f = (admin_fn)GetProcAddress(h, "IsNTAdmin"))) {
        printf("overmap: no advpack (%lu)\n", GetLastError());
        return 1;
    }
    r = f(0, NULL);
    printf("overmap: advpack at %p, IsNTAdmin at +%#lx returned %lu\n", (void *)h, (unsigned long)((char *)f - (char *)h), r);
    FreeLibrary(h);
    if (GetModuleHandleA("advpack.dll")) {
        printf("overmap: advpack still loaded\n");
        return 1;
    }
    file = CreateFileA("overdll.dll", GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, 0, NULL);
    if (file == INVALID_HANDLE_VALUE || !ReadFile(file, buf, sizeof(buf), &n, NULL)) {
        printf("overmap: cannot read overdll.dll (%lu)\n", GetLastError());
        return 1;
    }
    CloseHandle(file);
    nt = (IMAGE_NT_HEADERS *)(buf + ((IMAGE_DOS_HEADER *)buf)->e_lfanew);
    nt->OptionalHeader.ImageBase = (ULONG_PTR)h;
    GetTempPathA(sizeof(tmp), tmp);
    strcat(tmp, "overdll-at.dll");
    file = CreateFileA(tmp, GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE, FILE_SHARE_READ, NULL, CREATE_ALWAYS, 0, NULL);
    if (file == INVALID_HANDLE_VALUE || !WriteFile(file, buf, n, &n, NULL)) {
        printf("overmap: cannot write %s (%lu)\n", tmp, GetLastError());
        return 1;
    }
    map = CreateFileMappingA(file, NULL, PAGE_EXECUTE_READ | SEC_IMAGE, 0, 0, NULL);
    view = map ? MapViewOfFile(map, FILE_MAP_READ | FILE_MAP_EXECUTE, 0, 0, 0) : NULL;
    if (view != (void *)h) {
        MEMORY_BASIC_INFORMATION mbi = {0};
        VirtualQuery(h, &mbi, sizeof(mbi));
        printf("overmap: overdll mapped at %p, not at %p (%lu); there: base %p size %#lx state %#lx type %#lx protect %#lx\n", view,
               (void *)h, GetLastError(), mbi.AllocationBase, (unsigned long)mbi.RegionSize, mbi.State, mbi.Type, mbi.Protect);
        return 1;
    }
    r = f(0, NULL);
    if (r != 42) {
        printf("overmap stale: the call at advpack's old export returned %lu\n", r);
        return 1;
    }
    printf("overmap ok\n");
    return 0;
}
