/* A callee that reads the ZF its caller set, as MSVC's debug-CRT stack
 * check does (`cmp esi, esp; call [__imp___chkesp]`, whose `jne` reports a
 * bad stack): the function tier must write ZF back at an indirect call. */
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

__attribute__((noinline)) static int readzf(void) {
    int r;
    __asm__ volatile("setz %%al\n\tmovzbl %%al, %0" : "=r"(r) : : "eax", "cc");
    return r;
}
static int (*volatile fp)(void) = readzf;

/* As MSVC emits it around a call through a pointer in a debug build: esi
 * holds esp from before the call, and `cmp esi, esp; call __chkesp`
 * follows; here esi is off by `skew` bytes when it should not match. */
__attribute__((noinline)) static int equal(long skew) {
    int (*f)(void) = fp;
    int r;
#ifdef __i386__
    __asm__ volatile("push %%esi\n\t"
                     "mov %%esp, %%esi\n\t"
                     "add %2, %%esi\n\t"
                     "cmp %%esp, %%esi\n\t"
                     "call *%1\n\t"
                     "pop %%esi"
                     : "=a"(r)
                     : "r"(f), "r"(skew)
                     : "ecx", "edx", "cc", "memory");
#else
    r = skew == 0;
    (void)f;
#endif
    return r;
}

int main(int argc, char **argv) {
    (void)argv;
    int n = 0;
    for (int i = 0; i < 100000; i++) {
        n += equal(0) + 2 * equal(argc * 4);
    }
    CHECK("the callee sees the caller's ZF", n == 100000);
    printf("%d failures\n", failures);
    return failures != 0;
}
