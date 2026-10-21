/* An AOT-only wine32 session's faults (aotfault.sh).
 *
 *   aotfault.exe        "aotfault hello"
 *   aotfault.exe dep    calls into a page that is not executable; the
 *                       vectored handler sees an access violation,
 *                       executing (ExceptionInformation[0] = 8), and
 *                       prints "aotfault dep ok"
 *   aotfault.exe gen    calls code it wrote at run time, which an AOT-only
 *                       process cannot run: the run stops, saying so */
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
    return 1;
}
