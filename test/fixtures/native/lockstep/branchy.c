/* test/native/lockstep: compare-heavy code -- classification chains, clamps, signed/unsigned
 * range checks and a branchy binary search, on data that defeats branch prediction. */
#include "kernel.h"
#include <stdint.h>
enum { N = 8192 };
static int32_t data[N], sorted[N];
static uint32_t rng = 12345;
static uint32_t rnd(void) { rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5; return rng; }
__attribute__((noinline)) static int classify(int32_t v) {
    if (v < -1000) return v < -100000 ? 0 : 1;
    if (v < 0) return 2;
    if (v == 0) return 3;
    if (v <= 1000) return (v & 1) ? 4 : 5;
    if ((uint32_t)v >= 0x40000000u) return 6;
    return v % 3 == 0 ? 7 : 8;
}
__attribute__((noinline)) static int32_t clamp(int32_t v, int32_t lo, int32_t hi) { return v < lo ? lo : v > hi ? hi : v; }
__attribute__((noinline)) static int bsearch_le(const int32_t *s, int n, int32_t key) {
    int lo = 0, hi = n - 1, best = -1;
    while (lo <= hi) { int mid = (lo + hi) >> 1; if (s[mid] <= key) { best = mid; lo = mid + 1; } else hi = mid - 1; }
    return best;
}
unsigned kernel(int scale) {
    for (int i = 0; i < N; ++i) { uint32_t v = rnd(), k = rnd() & 31, neg = rnd() & 1; v >>= k; data[i] = (int32_t)(neg ? 0u - v : v); }
    for (int i = 0; i < N; ++i) sorted[i] = (i - N / 2) * 977;
    uint32_t hist[9] = { 0 }, acc = 0;
    for (int r = 0; r < 25 * scale; ++r) {
        for (int i = 0; i < N; ++i) {
            int32_t v = (int32_t)((uint32_t)data[i] + (uint32_t)r);
            hist[classify(v)]++;
            acc += (uint32_t)clamp(v >> 8, -5000, 5000);
            acc += (uint32_t)bsearch_le(sorted, N, v >> 12);
            if ((uint32_t)(v - 100) < 900u) acc ^= (uint32_t)v;
        }
    }
    for (int i = 0; i < 9; ++i) acc = acc * 31 + hist[i];
    return acc;
}
