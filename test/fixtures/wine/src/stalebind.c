/* A native Wine DLL's skeleton mapped as an image view, unmapped, and its
 * addresses reused for the program's own code (64-bit Wine native PE core, NATIVE=).
 *
 * The skeleton's exports are bound to Wine's PE module in the function
 * table. Once the view is gone those bindings must go too: kept, the code
 * that next lands at an export's address is sent to the native export, or
 * (its page written, the bridge forgotten) bridged to it again at every
 * call ("provider re-bridge"). FL Studio's page start did that at two
 * pages of anonymous code at 0x7fc11000 and 0x7fc31000 while the thread
 * held the loader lock, and every other thread waited on it.
 *
 * Here: kernelbase.dll mapped with SEC_IMAGE (as GetFileVersionInfo or a
 * resource load maps a DLL), the address of an export read from its
 * export table, the view unmapped, the same addresses allocated as
 * read-write-execute memory, a function written at the export's address,
 * then called many times. Each call must return the function's value; the
 * written code changes once, to be sure it is the program's code that
 * runs, not a stale translation of it either.
 * gex-nt-arch: x64
 */
#include <stdio.h>
#include <string.h>
#include <windows.h>

static DWORD export_rva(const unsigned char *base, const char *name) {
    const IMAGE_DOS_HEADER *dos = (const void *)base;
    const IMAGE_NT_HEADERS64 *nt = (const void *)(base + dos->e_lfanew);
    const IMAGE_DATA_DIRECTORY *dd = &nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_EXPORT];
    if (!dd->VirtualAddress) return 0;
    const IMAGE_EXPORT_DIRECTORY *ed = (const void *)(base + dd->VirtualAddress);
    const DWORD *names = (const void *)(base + ed->AddressOfNames);
    const WORD *ords = (const void *)(base + ed->AddressOfNameOrdinals);
    const DWORD *funcs = (const void *)(base + ed->AddressOfFunctions);
    for (DWORD i = 0; i < ed->NumberOfNames; i++)
        if (!strcmp((const char *)base + names[i], name)) {
            DWORD rva = funcs[ords[i]];
            /* a forwarder points into the export directory, not at code */
            if (rva >= dd->VirtualAddress && rva < dd->VirtualAddress + dd->Size) return 0;
            return rva;
        }
    return 0;
}

static void put_function(unsigned char *at, DWORD value) {
    at[0] = 0xb8; /* mov eax, imm32 */
    memcpy(at + 1, &value, 4);
    at[5] = 0xc3; /* ret */
    FlushInstructionCache(GetCurrentProcess(), at, 6);
}

/* The same calls from another thread: in the page, a thread's Worker has
 * instantiated none of the process's code yet when it first calls there. */
struct call { DWORD (*fn)(void); DWORD want; int bad; };
static DWORD WINAPI call_thread(void *p) {
    struct call *c = p;
    for (int i = 0; i < 2000; i++)
        if (c->fn() != c->want) c->bad++;
    return 0;
}

int main(void) {
    WCHAR path[MAX_PATH];
    GetSystemDirectoryW(path, MAX_PATH);
    wcscat(path, L"\\kernelbase.dll");
    HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, 0, NULL);
    if (f == INVALID_HANDLE_VALUE) { printf("open failed %lu\n", GetLastError()); return 1; }
    HANDLE sec = CreateFileMappingW(f, NULL, PAGE_READONLY | SEC_IMAGE, 0, 0, NULL);
    if (!sec) { printf("section failed %lu\n", GetLastError()); return 2; }
    int failures = 0;
    for (int round = 0; round < 3; round++) {
        unsigned char *view = MapViewOfFile(sec, FILE_MAP_READ, 0, 0, 0);
        if (!view) { printf("map failed %lu\n", GetLastError()); return 3; }
        DWORD rva = export_rva(view, "GetTickCount");
        if (!rva) rva = export_rva(view, "Sleep");
        if (!rva) { printf("no export\n"); return 4; }
        MEMORY_BASIC_INFORMATION mi;
        VirtualQuery(view, &mi, sizeof mi);
        SIZE_T size = 0;
        for (unsigned char *p = view; VirtualQuery(p, &mi, sizeof mi) && mi.AllocationBase == view; p += mi.RegionSize)
            size += mi.RegionSize;
        if (!UnmapViewOfFile(view)) { printf("unmap failed %lu\n", GetLastError()); return 5; }
        unsigned char *mem = VirtualAlloc(view, size, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE);
        if (mem != view) {
            printf("ok   round %d: the view's addresses were not free again (%p), nothing to check\n", round, mem);
            if (mem) VirtualFree(mem, 0, MEM_RELEASE);
            continue;
        }
        DWORD (*fn)(void) = (void *)(mem + rva);
        put_function(mem + rva, 1000 + round);
        int bad = 0;
        for (int i = 0; i < 2000; i++)
            if (fn() != 1000 + round) bad++;
        put_function(mem + rva, 2000 + round);
        for (int i = 0; i < 2000; i++)
            if (fn() != 2000 + round) bad++;
        struct call c = { fn, 2000 + round, 0 };
        HANDLE t = CreateThread(NULL, 0, call_thread, &c, 0, NULL);
        if (!t) { printf("thread failed %lu\n", GetLastError()); return 6; }
        WaitForSingleObject(t, INFINITE);
        CloseHandle(t);
        bad += c.bad;
        if (bad) { failures++; printf("FAIL round %d: %d calls at %p returned something else\n", round, bad, (void *)fn); }
        else printf("ok   round %d: the program's code at a former export runs\n", round);
        VirtualFree(mem, 0, MEM_RELEASE);
    }
    CloseHandle(sec);
    CloseHandle(f);
    printf("%d failures\n", failures);
    return failures != 0;
}
