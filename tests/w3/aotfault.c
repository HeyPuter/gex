/* An AOT-only wine32 session's faults (aotfault.sh).
 *
 *   aotfault.exe        "aotfault hello"
 *   aotfault.exe dep    calls into a page that is not executable; the
 *                       vectored handler sees an access violation,
 *                       executing (ExceptionInformation[0] = 8), and
 *                       prints "aotfault dep ok"
 *   aotfault.exe gen    calls code it wrote at run time, which an AOT-only
 *                       process cannot run: the run stops, saying so
 *   aotfault.exe thunk  calls through a window-procedure thunk of each
 *                       documented shape it wrote at run time (`mov dword
 *                       [esp+4], imm32; jmp`, `mov ecx, imm32; jmp`), which
 *                       an AOT-only process runs: "aotfault thunk ok"
 *   aotfault.exe near   calls through a thunk of neither shape (`mov dword
 *                       [esp+8], imm32; jmp`): the run stops, as for gen */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static LONG CALLBACK on_fault(EXCEPTION_POINTERS *e)
{
    EXCEPTION_RECORD *r = e->ExceptionRecord;
    if (r->ExceptionCode == EXCEPTION_ACCESS_VIOLATION && r->NumberParameters >= 2 &&
        r->ExceptionInformation[0] == 8) {
        printf("aotfault dep ok\n");
        fflush(stdout);
        ExitProcess(0);
    }
    printf("aotfault: exception %#lx, kind %lu\n", r->ExceptionCode,
           r->NumberParameters ? (unsigned long)r->ExceptionInformation[0] : 0ul);
    fflush(stdout);
    ExitProcess(3);
}

static __attribute__((noinline)) int __stdcall by_stack(int a, int b)
{
    return a + b;
}

static __attribute__((noinline)) int __fastcall by_ecx(int a, int b)
{
    return a + b;
}

/* At p: `op` (its bytes, then imm), then `jmp to`. */
static unsigned char *thunk(unsigned char *p, const unsigned char *op, int n, unsigned imm, void *to)
{
    memcpy(p, op, n);
    memcpy(p + n, &imm, 4);
    p[n + 4] = 0xe9;
    unsigned rel = (unsigned)to - (unsigned)(p + n + 9);
    memcpy(p + n + 5, &rel, 4);
    return p;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        printf("aotfault hello\n");
        return 0;
    }
    if (!strcmp(argv[1], "dep")) {
        AddVectoredExceptionHandler(1, on_fault);
        unsigned char *p = VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        p[0] = 0xc3; /* ret */
        ((void (*)(void))p)();
        printf("aotfault: the call returned\n");
        return 2;
    }
    if (!strcmp(argv[1], "gen")) {
        unsigned char *p = VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
        p[0] = 0xc3;
        ((void (*)(void))p)();
        printf("aotfault: generated code ran\n");
        return 2;
    }
    if (!strcmp(argv[1], "thunk") || !strcmp(argv[1], "near")) {
        static const unsigned char arg1[] = {0xc7, 0x44, 0x24, 0x04}, arg2[] = {0xc7, 0x44, 0x24, 0x08}, ecx[] = {0xb9};
        unsigned char *p = VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
        if (!strcmp(argv[1], "near")) {
            int r = ((int (__stdcall *)(int, int))thunk(p, arg2, 4, 0x500, (void *)by_stack))(1, 2);
            printf("aotfault: the near thunk ran (%#x)\n", r);
            return 2;
        }
        int r1 = ((int (__stdcall *)(int, int))thunk(p, arg1, 4, 0x5150, (void *)by_stack))(1, 2);
        int r2 = ((int (__fastcall *)(int, int))thunk(p + 64, ecx, 1, 0x7700, (void *)by_ecx))(1, 3);
        if (r1 == 0x5152 && r2 == 0x7703) printf("aotfault thunk ok\n");
        else printf("aotfault: thunks gave %#x %#x\n", r1, r2);
        return 0;
    }
    return 1;
}
