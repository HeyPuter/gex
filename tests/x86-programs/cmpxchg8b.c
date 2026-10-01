/* `lock cmpxchg8b` as one 64-bit atomic: what it leaves in edx:eax and ZF
 * either way, and four threads bumping a {count, count} pair through it --
 * the pointer-and-counter head of a lock-free list (Windows' SLIST_HEADER).
 * Done in two halves, the pair tears and increments are lost. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

static volatile uint64_t pair __attribute__((aligned(8)));

/* Returns ZF; *old gets edx:eax afterwards. */
static int cas8(volatile uint64_t *p, uint64_t expected, uint64_t want, uint64_t *old) {
    uint32_t lo = (uint32_t)expected, hi = (uint32_t)(expected >> 32);
    unsigned char z;
    __asm__ volatile("lock cmpxchg8b %0\n sete %1"
                     : "+m"(*p), "=q"(z), "+a"(lo), "+d"(hi)
                     : "b"((uint32_t)want), "c"((uint32_t)(want >> 32))
                     : "cc", "memory");
    *old = (uint64_t)hi << 32 | lo;
    return z;
}

#define N 200000

static void *bump(void *arg) {
    (void)arg;
    for (int i = 0; i < N; i++) {
        uint64_t seen = pair, old;
        for (;;) {
            uint32_t c = (uint32_t)seen + 1;
            if (cas8(&pair, seen, (uint64_t)c << 32 | c, &old)) break;
            seen = old;
        }
    }
    return 0;
}

int main(void) {
    uint64_t old;
    pair = 0x1111111122222222ull;
    int z = cas8(&pair, 0x1111111122222222ull, 0x3333333344444444ull, &old);
    printf("equal: zf=%d old=%016llx now=%016llx\n", z, (unsigned long long)old, (unsigned long long)pair);
    z = cas8(&pair, 0x5555555566666666ull, 0x7777777788888888ull, &old);
    printf("unequal: zf=%d old=%016llx now=%016llx\n", z, (unsigned long long)old, (unsigned long long)pair);

    pair = 0;
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, bump, 0);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    uint32_t lo = (uint32_t)pair, hi = (uint32_t)(pair >> 32);
    printf("threads: %s (%u/%u of %u)\n", lo == 4 * N && hi == lo ? "every increment" : "LOST OR TORN", lo, hi, 4 * N);
    return 0;
}
