/* Many small functions, called once each in an order that jumps about,
 * then all again in order: each first call misses at an address on a page
 * other blocks were already translated on. With GEX_TEST_DENSE_HOLES=1 in
 * the page, each such miss first finds its slot's
 * word pointing past the last block of the newest translation (a word left
 * stale), and must still reach the miss stub and be translated. */
#include <stdio.h>

#define F(n) __attribute__((noinline)) static unsigned f##n(unsigned x) { return x * (2u * n + 1u) + (n ^ (x >> 3)); }
#define F10(n) F(n##0) F(n##1) F(n##2) F(n##3) F(n##4) F(n##5) F(n##6) F(n##7) F(n##8) F(n##9)
#define F100(n) F10(n##0) F10(n##1) F10(n##2) F10(n##3) F10(n##4) F10(n##5) F10(n##6) F10(n##7) F10(n##8) F10(n##9)
F100(1) F100(2) F100(3) F100(4) F100(5) F100(6)

#define R(n) f##n,
#define R10(n) R(n##0) R(n##1) R(n##2) R(n##3) R(n##4) R(n##5) R(n##6) R(n##7) R(n##8) R(n##9)
#define R100(n) R10(n##0) R10(n##1) R10(n##2) R10(n##3) R10(n##4) R10(n##5) R10(n##6) R10(n##7) R10(n##8) R10(n##9)
static unsigned (*const fs[])(unsigned) = { R100(1) R100(2) R100(3) R100(4) R100(5) R100(6) };

int main(void) {
    const unsigned n = sizeof fs / sizeof fs[0];
    unsigned sum = 0;
    /* A step coprime to 600: every function once, scattered. */
    for (unsigned i = 0, k = 0; i < n; i++, k = (k + 7 * 37) % n) sum = sum * 31u + fs[k](i);
    for (int round = 0; round < 3; round++)
        for (unsigned i = 0; i < n; i++) sum = sum * 31u + fs[i](sum);
    printf("%u functions, sum %08x\n", n, sum);
    return 0;
}
