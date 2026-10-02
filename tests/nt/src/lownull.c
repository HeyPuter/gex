/* A locked access through a null pointer's field faults, as on Windows.
 * gex has no page protection below the guest's floor (its thread contexts
 * and dispatch directory live there), so such a write once landed in the
 * runtime's own memory -- a test calling msvcp120's _Mtx_lock with the
 * mutex instead of its address made ConcRT lock a critical_section at 0x9.
 * The misaligned-atomic path and cmpxchg16b check the address and raise
 * STATUS_ACCESS_VIOLATION (a write, at the address) through the exception
 * dispatcher.
 *
 *   lownull xadd | aligned | cmpxchg16b
 *
 * (xadd at 0x39 is misaligned, aligned at 0x40 is not.)
 *
 * A vectored handler takes the exception, prints one line and the failure
 * count, and ends the process with it (gex reports the fault at the start
 * of the translated block, so resuming is not attempted).
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static volatile UINT_PTR where = 0x39;
static UINT_PTR expected;
static const char *what;

static LONG CALLBACK handler(EXCEPTION_POINTERS *e) {
    EXCEPTION_RECORD *r = e->ExceptionRecord;
    ULONG_PTR i0 = r->NumberParameters >= 2 ? r->ExceptionInformation[0] : ~(ULONG_PTR)0;
    ULONG_PTR i1 = r->NumberParameters >= 2 ? r->ExceptionInformation[1] : ~(ULONG_PTR)0;
    int ok = r->ExceptionCode == EXCEPTION_ACCESS_VIOLATION && i0 == 1 && i1 == expected;
    printf("%s %s at %#zx: exception %#lx, write %zu, address %#zx\n", ok ? "ok" : "FAIL", what, (size_t)expected,
           (unsigned long)r->ExceptionCode, (size_t)i0, (size_t)i1);
    printf("%d failures\n", !ok);
    fflush(stdout);
    ExitProcess(!ok);
    return EXCEPTION_CONTINUE_SEARCH;
}

__attribute__((noinline)) static void locked_add(void) {
    InterlockedExchangeAdd((volatile LONG *)where, 1);
}

#ifdef _WIN64
__attribute__((noinline)) static void cmpxchg16b(void) {
    UINT64 lo = 0, hi = 0;
    __asm__ volatile("lock cmpxchg16b %0"
                     : "+m"(*(volatile char *)(where + 7)), "+a"(lo), "+d"(hi)
                     : "b"((UINT64)1), "c"((UINT64)1)
                     : "memory", "cc");
}
#endif

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    AddVectoredExceptionHandler(1, handler);
    what = argc > 1 ? argv[1] : "xadd";
    if (!strcmp(what, "xadd")) {
        expected = where;
        locked_add();
    } else if (!strcmp(what, "aligned")) {
        where = 0x40;
        expected = where;
        locked_add();
#ifdef _WIN64
    } else if (!strcmp(what, "cmpxchg16b")) {
        expected = where + 7;
        cmpxchg16b();
#endif
    } else {
        printf("FAIL unknown case %s\n1 failures\n", what);
        return 1;
    }
    printf("FAIL %s at %#zx did not fault\n1 failures\n", what, (size_t)expected);
    return 1;
}
