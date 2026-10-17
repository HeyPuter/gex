/* KUSER_SHARED_DATA read the way Windows programs read it: by constant
 * address at 0x7ffe0000 (GetTickCount inlined, the interrupt time, the OS
 * version), in every addressing form that carries an absolute displacement.
 * Under W3 (Wine as wasm32) the page is elsewhere (gex_abi::W3_KUSER) and
 * the translator folds these accesses to it; elsewhere they read the page
 * where it is.
 *
 *   kuser const      the constant reads, checked against the APIs
 *   kuser pointer    a pointer into the page computed at run time: under W3
 *                    an access violation (gex says "KUSER access not folded"),
 *                    which a vectored handler reports; elsewhere a plain read
 *
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#define KUSER 0x7ffe0000u
#define AT(off) (*(volatile ULONG *)(ULONG_PTR)(KUSER + (off)))

static int failures;

static void check(int ok, const char *what, unsigned long long got, unsigned long long want) {
    printf("%s %s: %#llx (want %#llx)\n", ok ? "ok" : "FAIL", what, got, want);
    failures += !ok;
}

/* KSYSTEM_TIME: LowPart, High1Time, High2Time; read until the highs agree. */
static ULONGLONG ksystem_time(ULONG off) {
    for (;;) {
        ULONG hi = AT(off + 4), lo = AT(off), hi2 = AT(off + 8);
        if (hi == hi2) return (ULONGLONG)hi << 32 | lo;
    }
}

static volatile int index_one = 1;
static volatile ULONG *volatile zero_base;

/* The forms are written out: a compiler may load the constant into a
 * register instead, which is a pointer, not a displacement. */
__attribute__((noinline)) static ULONG indexed(int i) {
    ULONG v;
    /* [index*4 + 0x7ffe031c]: an index with the displacement (i is 1) */
    __asm__ volatile("movl 0x7ffe031c(,%1,4), %0" : "=r"(v) : "r"((ULONG_PTR)i));
    return v;
}

__attribute__((noinline)) static ULONG based(volatile ULONG *base) {
    ULONG v;
    /* [base + 0x7ffe026c]: a base with the displacement (base is 0) */
    __asm__ volatile("movl 0x7ffe026c(%1), %0" : "=r"(v) : "r"(base));
    return v;
}

static volatile ULONG_PTR runtime_kuser;
static ULONG_PTR expected_addr;

static LONG CALLBACK handler(EXCEPTION_POINTERS *e) {
    EXCEPTION_RECORD *r = e->ExceptionRecord;
    ULONG_PTR i0 = r->NumberParameters >= 2 ? r->ExceptionInformation[0] : ~(ULONG_PTR)0;
    ULONG_PTR i1 = r->NumberParameters >= 2 ? r->ExceptionInformation[1] : ~(ULONG_PTR)0;
    int ok = r->ExceptionCode == EXCEPTION_ACCESS_VIOLATION && i0 == 0 && i1 == expected_addr;
    printf("%s pointer read at %#zx: exception %#lx, read %zu, address %#zx\n", ok ? "ok" : "FAIL",
           (size_t)expected_addr, (unsigned long)r->ExceptionCode, (size_t)i0, (size_t)i1);
    printf("%d failures\n", !ok);
    fflush(stdout);
    ExitProcess(!ok);
    return EXCEPTION_CONTINUE_SEARCH;
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "const";
    setvbuf(stdout, 0, _IONBF, 0);
    if (!strcmp(mode, "pointer")) {
        /* Not a constant the translator can see: a store and a load away. */
        runtime_kuser = KUSER;
        expected_addr = runtime_kuser + 0x26c;
        AddVectoredExceptionHandler(1, handler);
        ULONG v = *(volatile ULONG *)expected_addr;
        /* Reached where the page is really at 0x7ffe0000 (not W3). */
        printf("read %lu at %#zx\n", (unsigned long)v, (size_t)expected_addr);
        return 3;
    }

    /* The OS version, absolute ([disp32]) and through a base and an index. */
    OSVERSIONINFOW vi = {sizeof vi};
    LONG(WINAPI * get_version)(OSVERSIONINFOW *) = (void *)GetProcAddress(GetModuleHandleA("ntdll.dll"), "RtlGetVersion");
    get_version(&vi);
    check(AT(0x26c) == vi.dwMajorVersion, "NtMajorVersion", AT(0x26c), vi.dwMajorVersion);
    check(AT(0x270) == vi.dwMinorVersion, "NtMinorVersion", AT(0x270), vi.dwMinorVersion);
    ULONG b = based(zero_base);
    check(b == vi.dwMajorVersion, "NtMajorVersion through a base", b, vi.dwMajorVersion);

    /* The tick count, as GetTickCount reads it, within a second of the API. */
    ULONGLONG t0 = ksystem_time(0x320), mul = AT(0x4);
    ULONG ticks = (ULONG)((t0 * mul) >> 24), api = GetTickCount();
    check(ticks - api + 1000 <= 2000, "tick count", ticks, api);
    ULONG x = indexed(index_one), y = AT(0x320);
    check(y - x <= 1, "tick count through an index", x, y);

    /* The interrupt time moves. */
    ULONGLONG i0 = ksystem_time(0x8);
    Sleep(50);
    ULONGLONG i1 = ksystem_time(0x8);
    check(i0 != 0 && i1 > i0, "interrupt time advances", i1 - i0, 1);

    printf("%d failures\n", failures);
    return failures;
}
