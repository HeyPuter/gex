/* `lock cmpxchg16b` (x86-64): equal and unequal, and four threads bumping a
 * {count, count} pair of quadwords through it -- x64 Windows' SLIST_HEADER is
 * this. gex does it in the host under one lock (wasm has no 16-byte atomic);
 * done in two halves, the pair tears and increments are lost. On i386 there
 * is no such instruction: the program says so and stops. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

#ifdef __x86_64__
typedef struct { volatile uint64_t lo, hi; } __attribute__((aligned(16))) pair_t;
static pair_t pair;

static int cas16(pair_t *p, uint64_t elo, uint64_t ehi, uint64_t nlo, uint64_t nhi, uint64_t *olo, uint64_t *ohi) {
    unsigned char z;
    __asm__ volatile("lock cmpxchg16b %0\n sete %1"
                     : "+m"(*p), "=q"(z), "+a"(elo), "+d"(ehi)
                     : "b"(nlo), "c"(nhi)
                     : "cc", "memory");
    *olo = elo;
    *ohi = ehi;
    return z;
}

#define N 100000

static void *bump(void *arg) {
    (void)arg;
    for (int i = 0; i < N; i++) {
        uint64_t lo = pair.lo, hi = pair.hi, olo, ohi;
        while (!cas16(&pair, lo, hi, lo + 1, hi + 1, &olo, &ohi)) {
            lo = olo;
            hi = ohi;
        }
    }
    return 0;
}

int main(void) {
    uint64_t lo, hi;
    pair.lo = 0x1111; pair.hi = 0x2222;
    int z = cas16(&pair, 0x1111, 0x2222, 0x3333, 0x4444, &lo, &hi);
    printf("equal: zf=%d old=%llx/%llx now=%llx/%llx\n", z, (unsigned long long)hi, (unsigned long long)lo,
           (unsigned long long)pair.hi, (unsigned long long)pair.lo);
    z = cas16(&pair, 0x5555, 0x6666, 0x7777, 0x8888, &lo, &hi);
    printf("unequal: zf=%d old=%llx/%llx now=%llx/%llx\n", z, (unsigned long long)hi, (unsigned long long)lo,
           (unsigned long long)pair.hi, (unsigned long long)pair.lo);
    pair.lo = pair.hi = 0;
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, bump, 0);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    printf("threads: %s (%llu/%llu of %d)\n", pair.lo == 4 * N && pair.hi == pair.lo ? "every increment" : "LOST OR TORN",
           (unsigned long long)pair.lo, (unsigned long long)pair.hi, 4 * N);
    return 0;
}
#else
int main(void) {
    puts("no cmpxchg16b on i386");
    return 0;
}
#endif
