/* Where an image without DYNAMIC_BASE goes: at its linked base when that is
 * free (no ASLR for it), and when it is not, somewhere else, relocated by
 * ntdll with its header's ImageBase left as linked, as Windows and Wine do
 * (packers check their headers against it). fixedbase_dll.dll is linked at
 * 0x18000000. */
#include <windows.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static DWORD header_base(HMODULE m) {
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)((char *)m + ((IMAGE_DOS_HEADER *)m)->e_lfanew);
    return (DWORD)nt->OptionalHeader.ImageBase;
}

int main(int argc, char **argv) {
    char path[MAX_PATH];
    GetModuleFileNameA(0, path, sizeof path);
    strcpy(strrchr(path, '\\') + 1, "fixedbase_dll.dll");
    if (argc > 1) {
        /* Take its base first. */
        void *r = VirtualAlloc((void *)0x18000000, 0x10000, MEM_RESERVE, PAGE_NOACCESS);
        CHECK("its base is reserved", r == (void *)0x18000000);
        HMODULE m = LoadLibraryA(path);
        CHECK("it loads elsewhere", m && m != (HMODULE)0x18000000);
        if (m) {
            int *(*f)(void) = (void *)GetProcAddress(m, "fixedbase_ptr");
            int *v = (int *)GetProcAddress(m, "fixedbase_value");
            CHECK("relocated: its code finds its data", f && f() == v && *v == 1234);
            CHECK("its header still names the linked base", header_base(m) == 0x18000000);
        }
    } else {
        HMODULE m = LoadLibraryA(path);
        CHECK("it loads at its linked base", m == (HMODULE)0x18000000);
        if (m) CHECK("its header names it", header_base(m) == 0x18000000);
    }
    printf("%d failures\n", failures);
    return failures;
}
