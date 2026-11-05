/* The driver of test/native/lockstep's programs: each defines
 * `unsigned kernel(int scale)`, called once to warm up and once timed through
 * a volatile pointer (a fresh entry into code the first call tiered up);
 * prints "result <checksum>" and "kernel-us <microseconds>". */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static double now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}
unsigned kernel(int scale) __attribute__((noinline));
static unsigned (*volatile bench_kernel)(int) = kernel;
int main(int argc, char **argv) {
    int scale = argc > 1 ? atoi(argv[1]) : 1;
    volatile unsigned sink = bench_kernel(scale > 1 ? scale / 2 : scale);
    (void)sink;
    double start = now_us();
    unsigned result = bench_kernel(scale);
    double end = now_us();
    printf("result %u\nkernel-us %.0f\n", result, end - start);
    return 0;
}
