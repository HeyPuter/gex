/* W64.1b (docs/wine-native-x64.md §2): in a lowered W64 process, data uses
 * the whole 4 GiB while code stays below 2 GiB, gex's code reach. More than
 * 2 GiB of data is allocated, executable memory and images land low and
 * run, and a high data page made executable is refused (as a JIT would
 * try), not moved. Prints "ok ..." lines and "N failures".
 *
 * `highdata mem64`: the memory64 flavour (W64.7, §0.1), which has neither
 * limit: 5 GiB of data, some of it above 4 GiB, executable memory runs
 * wherever it lands, and a high data page made executable runs.
 *
 * Any arguments after the first are a command line run as a child process
 * once the checks are done, waited for, and its exit code printed ("child
 * exit N"): the memory flavour is per program (gex docs/mem64.md §5), so a
 * memory64 highdata's child is a 32-bit process unless it asks too.
 *
 *   x86_64-w64-mingw32-gcc -O1 -o highdata.exe highdata.c */
#include <windows.h>
#include <stdio.h>

static int failures;
static void check(int ok, const char *what)
{
    printf("%s %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) failures++;
}

#define CODE_LIMIT 0x80000000ull
#define LIMIT32 0x100000000ull
#define BLOCK (128u << 20)
#define MAX_BLOCKS 40

int main(int argc, char **argv)
{
    int mem64 = argc > 1 && !strcmp(argv[1], "mem64");
    /* 2560 MiB, or 5 GiB in the memory64 flavour */
    int want = mem64 ? 40 : 20;
    ULONG_PTR highmark = mem64 ? LIMIT32 : CODE_LIMIT;
    void *blocks[MAX_BLOCKS];
    int n = 0, high = 0;
    for (int i = 0; i < want; i++)
    {
        blocks[i] = VirtualAlloc(NULL, BLOCK, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
        if (!blocks[i]) break;
        n++;
        /* one byte at each end: it is memory */
        ((volatile char *)blocks[i])[0] = (char)i;
        ((volatile char *)blocks[i])[BLOCK - 1] = (char)i;
        if ((ULONG_PTR)blocks[i] >= highmark) high++;
    }
    int intact = 1;
    for (int i = 0; i < n; i++)
        if (((volatile char *)blocks[i])[0] != (char)i || ((volatile char *)blocks[i])[BLOCK - 1] != (char)i) intact = 0;
    if (mem64)
    {
        check(n == want && intact, "5120 MiB of data allocated and intact");
        check(high > 0, "data above 4 GiB");
    }
    else
    {
        check(n == want && intact, "2560 MiB of data allocated and intact");
        check(high > 0, "data above 2 GiB");
    }

    static const unsigned char ret42[] = { 0xb8, 0x2a, 0x00, 0x00, 0x00, 0xc3 };  /* mov eax, 42; ret */
    DWORD types[2] = { MEM_RESERVE | MEM_COMMIT, MEM_RESERVE | MEM_COMMIT | MEM_TOP_DOWN };
    for (int t = 0; t < 2; t++)
    {
        unsigned char *code = VirtualAlloc(NULL, 0x10000, types[t], PAGE_EXECUTE_READWRITE);
        int low = code && (mem64 || (ULONG_PTR)code + 0x10000 <= CODE_LIMIT);
        int ran = 0;
        if (code)
        {
            memcpy(code, ret42, sizeof ret42);
            FlushInstructionCache(GetCurrentProcess(), code, sizeof ret42);
            if (low) ran = ((int (*)(void))code)() == 42;
            VirtualFree(code, 0, MEM_RELEASE);
        }
        if (mem64) check(low && ran, t ? "top-down executable allocation runs" : "executable allocation runs");
        else check(low && ran, t ? "top-down executable allocation below 2 GiB runs" : "executable allocation below 2 GiB runs");
    }

    if (!mem64)
        check((ULONG_PTR)GetModuleHandleA(NULL) < CODE_LIMIT && (ULONG_PTR)GetModuleHandleA("ntdll.dll") < CODE_LIMIT
              && (ULONG_PTR)GetModuleHandleA("kernel32.dll") < CODE_LIMIT, "images below 2 GiB");

    /* the kernel side's addresses past 2 GiB: a file written from and read
     * into a top-down buffer (the very top of the space) */
    char *top = VirtualAlloc(NULL, 0x10000, MEM_RESERVE | MEM_COMMIT | MEM_TOP_DOWN, PAGE_READWRITE);
    int io = 0;
    if (top && (ULONG_PTR)top >= highmark)
    {
        HANDLE f = CreateFileA("highdata.tmp", GENERIC_READ | GENERIC_WRITE, 0, NULL, CREATE_ALWAYS,
                               FILE_FLAG_DELETE_ON_CLOSE, NULL);
        DWORD done = 0;
        for (int i = 0; i < 0x8000; i++) top[i] = (char)(i * 7);
        if (f != INVALID_HANDLE_VALUE && WriteFile(f, top, 0x8000, &done, NULL) && done == 0x8000
            && SetFilePointer(f, 0, NULL, FILE_BEGIN) == 0 && ReadFile(f, top + 0x8000, 0x8000, &done, NULL)
            && done == 0x8000 && !memcmp(top, top + 0x8000, 0x8000))
            io = 1;
        if (f != INVALID_HANDLE_VALUE) CloseHandle(f);
    }
    check(io, mem64 ? "a file written from and read into a top-down buffer above 4 GiB"
                    : "a file written from and read into a top-down buffer above 2 GiB");
    if (top) VirtualFree(top, 0, MEM_RELEASE);

    /* a JIT's way: write data, then make it code */
    void *hi = NULL;
    for (int i = 0; i < n; i++) if ((ULONG_PTR)blocks[i] >= highmark) { hi = blocks[i]; break; }
    DWORD old;
    if (mem64)
    {
        /* code above 4 GiB is code like any other (gex docs/mem64.md §3) */
        int ran = 0;
        if (hi)
        {
            memcpy(hi, ret42, sizeof ret42);
            if (VirtualProtect(hi, 0x1000, PAGE_EXECUTE_READ, &old))
            {
                FlushInstructionCache(GetCurrentProcess(), hi, sizeof ret42);
                ran = ((int (*)(void))hi)() == 42;
            }
        }
        check(ran, "a data page above 4 GiB made executable runs");
    }
    else
    {
        check(hi && !VirtualProtect(hi, 0x1000, PAGE_EXECUTE_READ, &old), "a high data page made executable is refused");
        MEMORY_BASIC_INFORMATION mbi = { 0 };
        check(hi && VirtualQuery(hi, &mbi, sizeof mbi) && mbi.Protect == PAGE_READWRITE, "and stays read-write data");
    }

    for (int i = 0; i < n; i++) VirtualFree(blocks[i], 0, MEM_RELEASE);
    printf("%d failures\n", failures);
    if (argc > 2)
    {
        char cmd[1024] = "";
        STARTUPINFOA si = { sizeof si };
        PROCESS_INFORMATION pi;
        DWORD code = 99;
        for (int i = 2; i < argc; i++)
            snprintf(cmd + strlen(cmd), sizeof cmd - strlen(cmd), "%s%s", i > 2 ? " " : "", argv[i]);
        fflush(stdout);
        if (!CreateProcessA(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi))
            printf("FAIL CreateProcess %lu\n", GetLastError());
        else
        {
            WaitForSingleObject(pi.hProcess, INFINITE);
            GetExitCodeProcess(pi.hProcess, &code);
            printf("child exit %lu\n", code);
        }
    }
    return failures;
}
