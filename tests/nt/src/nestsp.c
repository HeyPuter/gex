/* Two levels of system calls into Wine, each running a user callback, with
 * traps out of the inner level only: an exception raised and caught there,
 * a processor trap caught there (ud2), and new code there (a translation the
 * dispatch misses). Under W64 a unit's call into Wine runs on Wine's own
 * stack below the guest's (gex's direct entry), and each level puts Wine's
 * stack pointer back as it found it; the inner level's traps are caught by
 * its own loop, which must leave the outer level's frames as they were. Many
 * rounds, so a level that put the pointer back wrong (a few KiB lost each
 * time) runs Wine out of stack or overwrites a live frame; then a deep nest.
 * x86-64 only (clang has no __try for i686 here, as seh.c); the i686 build
 * says so and passes.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -municode */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#ifndef __x86_64__
int wmain(void) { printf("ok not on this target\n0 failures\n"); return 0; }
#else

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

#define ROUNDS 150
#define DEEP 24

static HWND win;
static int raised, faulted, ran_new, inner_runs, deepest;
static unsigned char *code;

typedef int (*fn)(void);

/* mov eax, imm32; ret */
static void emit(unsigned char *p, int v) {
    p[0] = 0xb8;
    memcpy(p + 1, &v, 4);
    p[5] = 0xc3;
}

/* Each trap in a function of its own, called inside the __try: clang's
 * mingw __try covers calls only, not an inline faulting instruction. */
__attribute__((noinline)) static void do_ud2(void) { __builtin_trap(); }
static volatile int zero;
__attribute__((noinline)) static void do_div(int n) { zero = n / zero; }
/* `nestsp.exe ud2|div|none`: only that processor trap (default both). */
static int want_ud2 = 1, want_div = 1;

static __attribute__((noinline)) void inner_traps(int round) {
    __try { RaiseException(0xe0000011, 0, 0, 0); } __except (GetExceptionCode() == 0xe0000011) { raised++; }
    int both = 0;
    if (want_ud2) __try { do_ud2(); } __except (GetExceptionCode() == EXCEPTION_ILLEGAL_INSTRUCTION) { both++; }
    else both++;
    if (want_div) __try { do_div(round); } __except (GetExceptionCode() == EXCEPTION_INT_DIVIDE_BY_ZERO) { both++; }
    else both++;
    if (both == 2) faulted++;
    /* New code every tenth round at the same address: its old translation is
     * stale (each a retranslation, slow in the single-thread build). */
    if (round % 10) return;
    DWORD old;
    VirtualProtect(code, 4096, PAGE_READWRITE, &old);
    emit(code, round + 1000);
    VirtualProtect(code, 4096, PAGE_EXECUTE_READ, &old);
    FlushInstructionCache(GetCurrentProcess(), code, 6);
    if (((fn)code)() == round + 1000) ran_new++;
}

static __attribute__((noinline)) ULONG_PTR sp_now(void) { return (ULONG_PTR)__builtin_frame_address(0); }

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    switch (m) {
    case WM_USER: {
        /* The outer level: its locals across the inner level must hold. */
        volatile ULONG_PTR mark[16];
        for (int i = 0; i < 16; i++) mark[i] = (ULONG_PTR)wp * 31 + i;
        ULONG_PTR sp0 = sp_now();
        LRESULT r = SendMessageW(w, WM_USER + 1, wp, 0);
        int same = sp_now() == sp0;
        for (int i = 0; i < 16; i++) same &= mark[i] == (ULONG_PTR)wp * 31 + i;
        return same ? r + 100 : -1;
    }
    case WM_USER + 1:
        inner_runs++;
        inner_traps((int)wp);
        return 7;
    case WM_USER + 2:
        /* A nest DEEP levels down, traps at the bottom. */
        if ((int)wp > deepest) deepest = (int)wp;
        if ((int)wp < DEEP) return SendMessageW(w, WM_USER + 2, wp + 1, 0) + 1;
        inner_traps(-1);
        return 0;
    }
    return DefWindowProcW(w, m, wp, lp);
}

int wmain(int argc, wchar_t **argv) {
    if (argc > 1) {
        want_ud2 = !wcscmp(argv[1], L"ud2");
        want_div = !wcscmp(argv[1], L"div");
    }
    WNDCLASSW wc = { .lpfnWndProc = proc, .hInstance = GetModuleHandleW(0), .lpszClassName = L"nestsp" };
    CHECK("RegisterClass", RegisterClassW(&wc) != 0);
    win = CreateWindowExW(0, L"nestsp", L"t", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, 0, 0, wc.hInstance, 0);
    CHECK("CreateWindowEx", win != 0);
    code = VirtualAlloc(0, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    CHECK("VirtualAlloc", code != 0);
    int good = 0;
    ULONG_PTR sp0 = sp_now();
    for (int round = 0; round < ROUNDS; round++) {
        LRESULT r = SendMessageW(win, WM_USER, round, 0);
        if (r == 107 && sp_now() == sp0) good++;
        else if (good == round) printf("round %d: %ld\n", round, (long)r);
    }
    CHECK("each round's outer level saw its inner level return, its frame intact", good == ROUNDS);
    CHECK("the inner level ran and trapped out every round",
          inner_runs == ROUNDS && raised == ROUNDS && faulted == ROUNDS && ran_new == ROUNDS / 10);
    if (!(inner_runs == ROUNDS && raised == ROUNDS && faulted == ROUNDS && ran_new == ROUNDS / 10))
        printf("inner %d raised %d faulted %d new code %d, of %d rounds\n", inner_runs, raised, faulted, ran_new, ROUNDS);
    LRESULT d = SendMessageW(win, WM_USER + 2, 1, 0);
    CHECK("a deep nest with traps at the bottom", d == DEEP - 1 && deepest == DEEP && raised == ROUNDS + 1);
    CHECK("and the outer level once more", SendMessageW(win, WM_USER, 5, 0) == 107);
    CHECK("DestroyWindow", DestroyWindow(win));
    printf("%d failures\n", failures);
    return failures != 0;
}
#endif
