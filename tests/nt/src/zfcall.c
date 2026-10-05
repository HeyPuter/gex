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

__attribute__((noinline)) static int equal(long a, long b) {
    int (*f)(void) = fp;
    int r;
    __asm__ volatile("cmp %2, %1\n\tcall *%3\n\tmov %%eax, %0"
                     : "=r"(r)
                     : "r"(a), "r"(b), "r"(f)
                     : "eax", "ecx", "edx", "cc", "memory");
    return r;
}

int main(int argc, char **argv) {
    (void)argv;
    int n = 0;
    for (int i = 0; i < 100000; i++) {
        n += equal(i, i) + 2 * equal(i, i + argc);
    }
    CHECK("the callee sees the caller's ZF", n == 100000);
    printf("%d failures\n", failures);
    return failures != 0;
}
