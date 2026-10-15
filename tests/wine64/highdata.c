/* W64.1b (docs/wine-native-x64.md §2): in a lowered W64 process, data uses
 * the whole 4 GiB while code stays below 2 GiB, gex's code reach. More than
 * 2 GiB of data is allocated, executable memory and images land low and
 * run, and a high data page made executable is refused (as a JIT would
 * try), not moved. Prints "ok ..." lines and "N failures".
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
#define BLOCK (128u << 20)
#define BLOCKS 20

int main(void)
{
    void *blocks[BLOCKS];
    int n = 0, high = 0;
    for (int i = 0; i < BLOCKS; i++)
    {
        blocks[i] = VirtualAlloc(NULL, BLOCK, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
        if (!blocks[i]) break;
        n++;
        /* one byte at each end: it is memory */
        ((volatile char *)blocks[i])[0] = (char)i;
        ((volatile char *)blocks[i])[BLOCK - 1] = (char)i;
        if ((ULONG_PTR)blocks[i] >= CODE_LIMIT) high++;
    }
    int intact = 1;
    for (int i = 0; i < n; i++)
        if (((volatile char *)blocks[i])[0] != (char)i || ((volatile char *)blocks[i])[BLOCK - 1] != (char)i) intact = 0;
    check(n == BLOCKS && intact, "2560 MiB of data allocated and intact");
    check(high > 0, "data above 2 GiB");

    static const unsigned char ret42[] = { 0xb8, 0x2a, 0x00, 0x00, 0x00, 0xc3 };  /* mov eax, 42; ret */
    DWORD types[2] = { MEM_RESERVE | MEM_COMMIT, MEM_RESERVE | MEM_COMMIT | MEM_TOP_DOWN };
    for (int t = 0; t < 2; t++)
    {
        unsigned char *code = VirtualAlloc(NULL, 0x10000, types[t], PAGE_EXECUTE_READWRITE);
        int low = code && (ULONG_PTR)code + 0x10000 <= CODE_LIMIT;
        int ran = 0;
        if (code)
        {
            memcpy(code, ret42, sizeof ret42);
            FlushInstructionCache(GetCurrentProcess(), code, sizeof ret42);
            if (low) ran = ((int (*)(void))code)() == 42;
            VirtualFree(code, 0, MEM_RELEASE);
        }
        check(low && ran, t ? "top-down executable allocation below 2 GiB runs" : "executable allocation below 2 GiB runs");
    }

    check((ULONG_PTR)GetModuleHandleA(NULL) < CODE_LIMIT && (ULONG_PTR)GetModuleHandleA("ntdll.dll") < CODE_LIMIT
          && (ULONG_PTR)GetModuleHandleA("kernel32.dll") < CODE_LIMIT, "images below 2 GiB");

    /* a JIT's way: write data, then make it code */
    void *hi = NULL;
    for (int i = 0; i < n; i++) if ((ULONG_PTR)blocks[i] >= CODE_LIMIT) { hi = blocks[i]; break; }
    DWORD old;
    check(hi && !VirtualProtect(hi, 0x1000, PAGE_EXECUTE_READ, &old), "a high data page made executable is refused");
    MEMORY_BASIC_INFORMATION mbi = { 0 };
    check(hi && VirtualQuery(hi, &mbi, sizeof mbi) && mbi.Protect == PAGE_READWRITE, "and stays read-write data");

    for (int i = 0; i < n; i++) VirtualFree(blocks[i], 0, MEM_RELEASE);
    printf("%d failures\n", failures);
    return failures;
}
