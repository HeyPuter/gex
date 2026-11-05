/* For web/test/threads-shared.mjs: a process that becomes threaded, then
 * runs code it has not run before.
 *
 * The page keeps the modules it translates for a joining Worker only once
 * the process has a second thread (`Jit::ever_threaded`); a single-threaded
 * run keeps none. So the clone comes first, and the work after it
 * (printf, and the loop over functions below) is what a Worker joining
 * later replays. The thread itself only exits: the test hands it to no
 * Worker, and the process's exit_group ends it. */
#include <stdio.h>
#include <stdlib.h>

static char stack[16384] __attribute__((aligned(16)));

static long clone_thread(void) {
    register long x0 __asm__("x0") = 0x00010f00; /* VM|FS|FILES|SIGHAND|THREAD|SYSVSEM */
    register long x1 __asm__("x1") = (long)(stack + sizeof stack);
    register long x2 __asm__("x2") = 0, x3 __asm__("x3") = 0, x4 __asm__("x4") = 0;
    register long x8 __asm__("x8") = 220; /* clone */
    __asm__ volatile(
        "svc #0\n"
        "cbnz x0, 1f\n"
        "mov x0, #0\n"
        "mov x8, #93\n" /* exit: this thread only */
        "svc #0\n"
        "1:\n"
        : "+r"(x0)
        : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x8)
        : "memory", "cc");
    return x0;
}

#define STEP(n) \
    __attribute__((noinline)) static unsigned step##n(unsigned v) { return v * (2 * n + 1) + n; }
STEP(1) STEP(2) STEP(3) STEP(4) STEP(5) STEP(6) STEP(7) STEP(8)

int main(void) {
    long tid = clone_thread();
    if (tid <= 0) {
        printf("clone failed: %ld\n", tid);
        return 1;
    }
    unsigned (*steps[])(unsigned) = {step1, step2, step3, step4, step5, step6, step7, step8};
    unsigned v = 1;
    for (int r = 0; r < 100; r++)
        for (int i = 0; i < 8; i++) v = steps[i](v);
    printf("threads-shared %u\n", v);
    exit(0);
}
