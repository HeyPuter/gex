/* test/native/lockstep: Ackermann -- deep, non-tail recursion with two arguments. */
#include "kernel.h"
__attribute__((noinline)) static unsigned ack(unsigned m, unsigned n) {
    if (m == 0) return n + 1;
    if (n == 0) return ack(m - 1, 1);
    return ack(m - 1, ack(m, n - 1));
}
unsigned kernel(int scale) {
    unsigned acc = 0;
    for (int i = 0; i < scale; ++i) acc = acc * 31 + ack(3, 7 + (i & 1)) + ack(2, 500 + i);
    return acc;
}
