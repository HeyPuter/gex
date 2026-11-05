/* A program without NX_COMPAT on 32-bit Wine (test/wow64/forceexec,
 * linked --disable-nxcompat): its loader opts the process out of DEP, which
 * makes every view executable. Code already bound in those views (Wine's
 * own exports) must keep running: the program then loads and calls into
 * DLLs, makes a (message-only) window and runs code it wrote itself. Prints
 * "forceexec ok", or "forceexec FAIL ...". */
#include <windows.h>
#include <shlwapi.h>
#include <stdio.h>

int main(void)
{
    ULONG flags = 0;
    char path[MAX_PATH] = "C:\\a\\b\\c.txt";
    static const unsigned char ret42[] = { 0xb8, 42, 0, 0, 0, 0xc3 };  /* mov eax, 42; ret */
    unsigned char *code;
    HWND hwnd;
    NTSTATUS (WINAPI *query)(HANDLE, ULONG, void *, ULONG, ULONG *) =
        (void *)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtQueryInformationProcess");

    if (query) query(GetCurrentProcess(), 0x22 /* ProcessExecuteFlags */, &flags, sizeof(flags), NULL);
    printf("forceexec: execute flags %#lx\n", flags);
    if (!(flags & 2 /* MEM_EXECUTE_OPTION_ENABLE */)) { printf("forceexec FAIL not opted out of DEP\n"); return 1; }
    PathRemoveFileSpecA(path);
    if (strcmp(path, "C:\\a\\b")) { printf("forceexec FAIL PathRemoveFileSpec %s\n", path); return 1; }
    if (!(hwnd = CreateWindowA("STATIC", "forceexec", 0, 0, 0, 10, 10, HWND_MESSAGE, NULL, NULL, NULL)))
    { printf("forceexec FAIL CreateWindow %lu\n", GetLastError()); return 1; }
    DestroyWindow(hwnd);
    /* data the program writes and runs, as a DEP opt-out allows: a page it
     * allocated read-write (forced executable) */
    if (!(code = VirtualAlloc(NULL, 4096, MEM_COMMIT, PAGE_READWRITE)))
    { printf("forceexec FAIL VirtualAlloc %lu\n", GetLastError()); return 1; }
    memcpy(code, ret42, sizeof(ret42));
    printf("forceexec: written code returns %d\n", ((int (*)(void))code)());
    printf("forceexec ok\n");
    return 0;
}
