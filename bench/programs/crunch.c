/* Integer loops of the kinds gex has historically been worst at.
 *
 * Sorting, hashing and bit twiddling: short bodies, data-dependent branches,
 * and no floating point or vectors to hide behind. nbench's numeric sort,
 * bitfield and IDEA kernels all live in this territory, and it is where gex
 * trails QEMU.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Small enough to stay in cache. This started at 20000 longs -- a 160 KB
   working set -- and on a shared machine that makes the benchmark a measure
   of how much cache everything else is using: two identical builds compared
   against each other read anywhere from 0.94 to 1.00. The instruction mix is
   what this program is for, not the memory traffic, so the array shrinks and
   the round count rises to keep the same amount of work. */
#define N 2048

static uint64_t heapsort_sum(uint64_t seed, int rounds) {
    static long a[N];
    uint64_t h = 0;
    for (int r = 0; r < rounds; r++) {
        for (int i = 0; i < N; i++) {
            seed = seed * 6364136223846793005ULL + 1442695040888963407ULL;
            a[i] = (long)(seed >> 33);
        }
        /* Sift-down heapsort: a branchy inner loop with a data-dependent
           trip count, which is the hard case. */
        for (int i = N / 2 - 1; i >= 0; i--) {
            int root = i;
            for (;;) {
                int child = 2 * root + 1;
                if (child >= N) break;
                if (child + 1 < N && a[child] < a[child + 1]) child++;
                if (a[root] >= a[child]) break;
                long t = a[root]; a[root] = a[child]; a[child] = t;
                root = child;
            }
        }
        for (int end = N - 1; end > 0; end--) {
            long t = a[0]; a[0] = a[end]; a[end] = t;
            int root = 0;
            for (;;) {
                int child = 2 * root + 1;
                if (child >= end) break;
                if (child + 1 < end && a[child] < a[child + 1]) child++;
                if (a[root] >= a[child]) break;
                long s = a[root]; a[root] = a[child]; a[child] = s;
                root = child;
            }
        }
        for (int i = 0; i < N; i++) h = h * 131 + (uint64_t)a[i];
    }
    return h;
}

static uint64_t bitfield(int rounds) {
    static uint32_t field[N / 4];
    uint64_t h = 0;
    for (int r = 0; r < rounds; r++) {
        memset(field, 0, sizeof field);
        uint32_t p = 12345;
        for (int i = 0; i < 200000; i++) {
            p = p * 1103515245u + 12345u;
            unsigned bit = p % (N * 8);
            field[bit >> 5] ^= 1u << (bit & 31);
        }
        for (unsigned i = 0; i < N / 4; i++) h += __builtin_popcount(field[i]);
    }
    return h;
}

/* IDEA's multiply-modulo, which is a branch on a 16-bit product. */
static uint16_t mul16(uint16_t a, uint16_t b) {
    uint32_t p = (uint32_t)a * b;
    if (p) {
        uint16_t lo = p & 0xffff, hi = p >> 16;
        return lo - hi + (lo < hi);
    }
    return 1 - a - b;
}

static uint64_t crypt(int rounds) {
    uint16_t key[52];
    for (int i = 0; i < 52; i++) key[i] = (uint16_t)(i * 2654 + 1);
    uint64_t h = 0;
    for (int r = 0; r < rounds; r++) {
        uint16_t x1 = 0x1234, x2 = 0x5678, x3 = 0x9abc, x4 = 0xdef0;
        for (int i = 0; i < 8; i++) {
            x1 = mul16(x1, key[i * 6 + 0]);
            x2 += key[i * 6 + 1];
            x3 += key[i * 6 + 2];
            x4 = mul16(x4, key[i * 6 + 3]);
            uint16_t t0 = mul16(x1 ^ x3, key[i * 6 + 4]);
            uint16_t t1 = mul16(t0 + (x2 ^ x4), key[i * 6 + 5]);
            t0 += t1;
            x1 ^= t1; x4 ^= t0;
            uint16_t s = x2; x2 = x3 ^ t1; x3 = s ^ t0;
        }
        h = h * 1099511628211ULL + ((uint64_t)x1 << 48 | (uint64_t)x2 << 32 |
                                    (uint64_t)x3 << 16 | x4);
    }
    return h;
}

int main(int argc, char **argv) {
    int scale = argc > 1 ? atoi(argv[1]) : 1;
    printf("%llu\n", (unsigned long long)heapsort_sum(1, 800 * scale));
    printf("%llu\n", (unsigned long long)bitfield(2000 * scale));
    printf("%llu\n", (unsigned long long)crypt(4000000 * scale));
    return 0;
}
