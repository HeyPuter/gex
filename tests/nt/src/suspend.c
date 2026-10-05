/* SuspendThread / GetThreadContext / ResumeThread on a running thread, the
 * way a stop-the-world GC uses them (Mono's Boehm GC on Windows, which
 * aborts when SuspendThread fails): a thread spinning in a loop and one
 * blocked in a wait are each suspended, their registers read, and resumed.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static volatile LONG counter, stop;
static char *spin_lo, *spin_hi;

static DWORD WINAPI spinner(void *p) {
    char here;
    spin_hi = &here + 4096;
    spin_lo = &here - 256 * 1024;
    while (!stop) InterlockedIncrement(&counter);
    return 7;
}

static HANDLE never;
static DWORD WINAPI waiter(void *p) {
    WaitForSingleObject(never, INFINITE);
    return 9;
}

static ULONG_PTR sp_of(CONTEXT *c) {
#ifdef _WIN64
    return c->Rsp;
#else
    return c->Esp;
#endif
}

int main(void) {
    HANDLE t = CreateThread(NULL, 0, spinner, NULL, 0, NULL);
    while (counter < 1000) Sleep(1);
    DWORD prev = SuspendThread(t);
    CHECK("SuspendThread on a running thread succeeds", prev == 0);
    CONTEXT c = {0};
    c.ContextFlags = CONTEXT_FULL;
    BOOL got = GetThreadContext(t, &c);
    CHECK("GetThreadContext on it succeeds", got);
    CHECK("its stack pointer is on its stack", sp_of(&c) >= (ULONG_PTR)spin_lo && sp_of(&c) < (ULONG_PTR)spin_hi);
    if (!(sp_of(&c) >= (ULONG_PTR)spin_lo && sp_of(&c) < (ULONG_PTR)spin_hi)) printf("  sp %p, stack %p..%p\n", (void *)sp_of(&c), spin_lo, spin_hi);
    LONG a = counter;
    Sleep(50);
    CHECK("it does not run while suspended", counter == a);
    CHECK("a second suspend counts", SuspendThread(t) == 1);
    CHECK("the first resume leaves it suspended", ResumeThread(t) == 2 && (Sleep(20), counter == a));
    CHECK("the last resume runs it again", ResumeThread(t) == 1);
    while (counter == a) Sleep(1);
    CHECK("it runs again", counter != a);

    never = CreateEventA(NULL, TRUE, FALSE, NULL);
    HANDLE w = CreateThread(NULL, 0, waiter, NULL, 0, NULL);
    Sleep(50);
    CHECK("SuspendThread on a waiting thread succeeds", SuspendThread(w) == 0);
    c.ContextFlags = CONTEXT_FULL;
    CHECK("GetThreadContext on it succeeds", GetThreadContext(w, &c));
    CHECK("ResumeThread on it", ResumeThread(w) == 1);
    SetEvent(never);
    DWORD code = 0;
    CHECK("the waiter finishes", WaitForSingleObject(w, 5000) == WAIT_OBJECT_0 && GetExitCodeThread(w, &code) && code == 9);

    stop = 1;
    CHECK("the spinner finishes", WaitForSingleObject(t, 5000) == WAIT_OBJECT_0 && GetExitCodeThread(t, &code) && code == 7);
    printf("%d failures\n", failures);
    return failures != 0;
}
