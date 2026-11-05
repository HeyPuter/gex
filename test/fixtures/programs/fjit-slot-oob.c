/* A function the function JIT translates holds a `call [0x7ff00000]` (2 GiB
 * less 1 MiB, far past a small program's memory) on a path it never takes
 * (test/page/fjit-slot-oob): the program must run to its end. */
#include <stdio.h>
__attribute__((noinline)) static long f(long n) {
    if (n < 0)
        __asm__ volatile("call *0x7ff00000" ::: "memory", "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "cc");
    return n * 3 + 1;
}
int main(int argc, char **argv) {
    (void)argv;
    setvbuf(stdout, 0, _IONBF, 0);
    long s = 0;
    for (long i = 0; i < 200000; i++) s += f(i + argc - 1);
    long want = 3L * (199999L * 200000L / 2) + 200000L;
    printf("%s: %ld calls of a function holding call [0x7ff00000], sum %ld, want %ld\n", s == want ? "ok" : "FAIL", 200000L, s, want);
    return s != want;
}
